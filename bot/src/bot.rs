use std::{str::FromStr, sync::Arc};

use anyhow::{anyhow, Context, Result};
use bigdecimal::num_bigint::{BigInt, ToBigInt};
use bson::{doc, Bson};
use chrono::{Duration, Utc};
use futures::TryStreamExt;
use mongodb::options::FindOneOptions;
use starknet::core::types::{BlockTag, FunctionCall};
use starknet::{
    accounts::{Account, Call, SingleOwnerAccount},
    core::types::{BlockId, FieldElement},
    macros::selector,
    providers::{jsonrpc::HttpTransport, JsonRpcClient, Provider},
    signers::LocalWallet,
};
use starknet_id::encode;

use crate::logger::Logger;
use crate::models::{
    AggregateResult, AggregateResults, DomainAggregateResult, MetadataDoc, Unzip5,
};
use crate::starknet_utils::create_jsonrpc_client;
use crate::utils::{hex_to_bigdecimal, to_uint256};
use crate::{
    config::Config,
    models::{AppState, Domain},
};
use bigdecimal::BigDecimal;

lazy_static::lazy_static! {
    static ref RENEW_TIME: FieldElement = FieldElement::from_dec_str("365").unwrap();
}

pub async fn get_domains_ready_for_renewal(
    config: &Config,
    state: &Arc<AppState>,
    logger: &Logger,
) -> Result<AggregateResults> {
    let domains = state.db.collection::<Domain>("domains");
    let min_expiry_date = Utc::now() + Duration::days(30);

    // Define aggregate pipeline
    let pipeline = vec![
        doc! { "$match": { "_cursor.to": null } },
        doc! { "$match": { "expiry": { "$lt": Bson::Int64(min_expiry_date.timestamp_millis() / 1000) } } },
        doc! { "$lookup": {
            "from": "auto_renew_flows",
            "let": { "domain_name": "$domain" },
            "pipeline": [
                { "$match":
                    { "$expr":
                        { "$and": [
                            { "$eq": [ "$domain",  "$$domain_name" ] },
                            { "$eq": [ { "$ifNull": [ "$_cursor.to", null ] }, null ] },
                        ]}
                    }
                }
            ],
            "as": "renewal_info",
        }},
        doc! { "$unwind": "$renewal_info" },
        doc! { "$lookup": {
            "from": "auto_renew_approvals",
            "let": { "renewer_addr": "$renewal_info.renewer_address" },
            "pipeline": [
                { "$match":
                    { "$expr":
                        { "$and": [
                            { "$eq": [ "$renewer",  "$$renewer_addr" ] },
                            { "$eq": [ { "$ifNull": [ "$_cursor.to", null ] }, null ] },
                        ]}
                    }
                }
            ],
            "as": "approval_info",
        }},
        doc! { "$unwind": { "path": "$approval_info", "preserveNullAndEmptyArrays": true } },
        doc! { "$project": {
            "domain": 1,
            "expiry": 1,
            "renewer_address": "$renewal_info.renewer_address",
            "enabled": "$renewal_info.enabled",
            "approval_value": { "$ifNull": [ "$approval_info.allowance", "0x0" ] },
            "allowance": "$renewal_info.allowance",
            "last_renewal": "$renewal_info.last_renewal",
            "meta_hash": "$renewal_info.meta_hash",
            "_cursor": "$renewal_info._cursor",
        }},
    ];

    // Execute the pipeline
    let cursor = domains.aggregate(pipeline, None).await?;
    // Extract the results as Vec<bson::Document>
    let bson_docs: Vec<bson::Document> = cursor.try_collect().await?;
    // Convert each bson::Document into DomainAggregateResult
    let results: Result<Vec<DomainAggregateResult>, _> = bson_docs
        .into_iter()
        .map(|doc| bson::from_bson(bson::Bson::Document(doc)))
        .collect();
    // Check if the conversion was successful
    let results = results?;

    // Then process the results
    let futures: Vec<_> = results
        .into_iter()
        .map(|result| process_aggregate_result(state, result, config, logger))
        .collect();

    let processed_results: Vec<_> = futures::future::try_join_all(futures)
        .await?
        .into_iter()
        .flatten()
        .collect();

    let (domains, renewers, domain_prices, tax_prices, meta_hashes): (
        Vec<FieldElement>,
        Vec<FieldElement>,
        Vec<BigDecimal>,
        Vec<BigDecimal>,
        Vec<FieldElement>,
    ) = processed_results
        .into_iter()
        .map(|res| {
            (
                res.domain,
                res.renewer_addr,
                res.domain_price,
                res.tax_price,
                res.meta_hash,
            )
        })
        .unzip5();

    Ok(AggregateResults {
        domains,
        renewers,
        domain_prices,
        tax_prices,
        meta_hashes,
    })
}

async fn check_user_balance(
    config: &Config,
    provider: JsonRpcClient<HttpTransport>,
    addr: FieldElement,
    allowance: BigDecimal,
) -> Result<Option<bool>> {
    let call_balance = provider
        .call(
            FunctionCall {
                contract_address: config.contract.erc20,
                entry_point_selector: selector!("balanceOf"),
                calldata: vec![addr],
            },
            BlockId::Tag(BlockTag::Latest),
        )
        .await;

    match call_balance {
        Ok(balance) => {
            let balance = BigDecimal::from_str(&FieldElement::to_string(&balance[0])).unwrap();
            Ok(Some(balance >= allowance))
        }
        Err(e) => Err(anyhow::anyhow!(
            "Error while fetching balance of user {:?} : {:?}",
            &addr,
            e
        )),
    }
}

async fn process_aggregate_result(
    state: &Arc<AppState>,
    result: DomainAggregateResult,
    config: &Config,
    logger: &Logger,
) -> Result<Option<AggregateResult>> {
    // Skip the rest if auto-renewal is not enabled
    if !result.enabled || result.allowance.is_none() {
        return Ok(None);
    }

    let renewer_addr = FieldElement::from_hex_be(&result.renewer_address)?;
    let erc20_allowance = if let Some(approval_value) = result.approval_value {
        hex_to_bigdecimal(&approval_value).unwrap()
    } else {
        BigDecimal::from(0)
    };
    let allowance = hex_to_bigdecimal(&result.allowance.unwrap()).unwrap();

    // get renew price from contract
    let provider = create_jsonrpc_client(&config);
    let domain_name = result
        .domain
        .strip_suffix(".stark")
        .ok_or_else(|| anyhow::anyhow!("Invalid domain name: {:?}", result.domain))?;
    let domain_len = domain_name.len();

    let call_result = provider
        .call(
            FunctionCall {
                contract_address: config.contract.pricing,
                entry_point_selector: selector!("compute_renew_price"),
                calldata: vec![domain_len.into(), *RENEW_TIME],
            },
            BlockId::Tag(BlockTag::Latest),
        )
        .await;

    match call_result {
        Ok(price) => {
            let price_str = FieldElement::to_string(&price[1]);
            let renew_price = BigDecimal::from_str(&price_str).unwrap();

            // Check user meta hash
            let mut tax_price = BigDecimal::from(0);
            let mut meta_hash = FieldElement::from_str("0")?;
            if let Some(hash) = result.meta_hash {
                meta_hash = FieldElement::from_hex_be(&hash)?;
                if hash != "0" {
                    let decimal_meta_hash =
                        BigInt::parse_bytes(hash.trim_start_matches("0x").as_bytes(), 16).unwrap();
                    let hex_meta_hash = decimal_meta_hash.to_str_radix(16);
                    let metadata_collection =
                        state.db_metadata.collection::<MetadataDoc>("metadata");
                    if let Some(document) = metadata_collection
                        .find_one(doc! {"meta_hash": hex_meta_hash}, FindOneOptions::default())
                        .await?
                    {
                        let tax_state = document.tax_state;
                        if let Some(state_info) = state.states.states.get(&tax_state) {
                            let tax_rate = (state_info.rate * 100.0).round() as i32;
                            tax_price = (renew_price.clone() * BigDecimal::from(tax_rate))
                                / BigDecimal::from(100);
                        }
                    }
                }
            }
            let final_price = renew_price.clone() + tax_price.clone();
            // Check user ETH allowance is greater or equal than final price = renew_price + tax_price
            if erc20_allowance >= final_price {
                // check user allowance is greater or equal than final price
                if allowance >= final_price {
                    // check user balance is sufficiant
                    let has_funds =
                        check_user_balance(config, provider, renewer_addr, final_price.clone())
                            .await?;
                    if let Some(false) = has_funds {
                        logger.warning(format!(
                            "Domain {} cannot be renewed because {} has not enough balance",
                            result.domain, result.renewer_address
                        ));
                        return Ok(None);
                    }

                    // encode domain name
                    let domain_encoded = encode(domain_name)
                        .map_err(|_| anyhow!("Failed to encode domain name"))
                        .context("Error occurred while encoding domain name")?;

                    Ok(Some(AggregateResult {
                        domain: domain_encoded,
                        renewer_addr,
                        domain_price: renew_price,
                        tax_price,
                        meta_hash,
                    }))
                } else {
                    logger.warning(format!(
                        "Domain {} cannot be renewed because {} has set an allowance({}) lower than final price({})",
                        result.domain, result.renewer_address, allowance, final_price
                    ));
                    Ok(None)
                }
            } else {
                logger.warning(format!(
                    "Domain {} cannot be renewed because {} has set an allowance ({}) lower than domain price({}) + tax({})",
                    result.domain, result.renewer_address, final_price, renew_price, tax_price
                ));
                Ok(None)
            }
        }
        Err(e) => Err(anyhow::anyhow!(
            "Error while fetching renew price for domain {:?} : {:?}",
            &result.domain,
            e
        )),
    }
}

pub async fn renew_domains(
    config: &Config,
    account: &SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>,
    mut aggregate_results: AggregateResults,
    logger: &Logger,
) -> Result<()> {
    // If we have more than 400 domains to renew we make multiple transactions to avoid hitting the 2M steps limit
    while !aggregate_results.domains.is_empty()
        && !aggregate_results.renewers.is_empty()
        && !aggregate_results.domain_prices.is_empty()
        && !aggregate_results.tax_prices.is_empty()
        && !aggregate_results.meta_hashes.is_empty()
    {
        let size = aggregate_results.domains.len().min(400);
        let domains_to_renew: Vec<FieldElement> =
            aggregate_results.domains.drain(0..size).collect();
        let renewers: Vec<FieldElement> = aggregate_results.renewers.drain(0..size).collect();
        let domain_prices: Vec<BigDecimal> =
            aggregate_results.domain_prices.drain(0..size).collect();
        let tax_prices: Vec<BigDecimal> = aggregate_results.tax_prices.drain(0..size).collect();
        let meta_hashes: Vec<FieldElement> = aggregate_results.meta_hashes.drain(0..size).collect();

        match send_transaction(
            config,
            account,
            AggregateResults {
                domains: domains_to_renew.clone(),
                renewers,
                domain_prices,
                tax_prices,
                meta_hashes,
            },
        )
        .await
        {
            Ok(_) => {
                logger.info(format!(
                    "Sent a tx to renew {} domains",
                    domains_to_renew.len()
                ));
            }
            Err(e) => {
                logger.severe(format!(
                    "Error while renewing domains: {:?} for domains: {:?}",
                    e, domains_to_renew
                ));
                return Err(e);
            }
        }
    }
    Ok(())
}

pub async fn send_transaction(
    config: &Config,
    account: &SingleOwnerAccount<JsonRpcClient<HttpTransport>, LocalWallet>,
    aggregate_results: AggregateResults,
) -> Result<()> {
    let mut calldata: Vec<FieldElement> = Vec::new();
    calldata
        .push(FieldElement::from_dec_str(&aggregate_results.domains.len().to_string()).unwrap());
    calldata.extend_from_slice(&aggregate_results.domains);
    calldata
        .push(FieldElement::from_dec_str(&aggregate_results.renewers.len().to_string()).unwrap());
    calldata.extend_from_slice(&aggregate_results.renewers);
    calldata.push(
        FieldElement::from_dec_str(&aggregate_results.domain_prices.len().to_string()).unwrap(),
    );

    for limit_price in &aggregate_results.domain_prices {
        let (low, high) = to_uint256(limit_price.to_bigint().unwrap());
        calldata.push(low);
        calldata.push(high);
    }

    calldata
        .push(FieldElement::from_dec_str(&aggregate_results.tax_prices.len().to_string()).unwrap());
    for tax_price in &aggregate_results.tax_prices {
        let (low, high) = to_uint256(tax_price.to_bigint().unwrap());
        calldata.push(low);
        calldata.push(high);
    }
    calldata.push(
        FieldElement::from_dec_str(&aggregate_results.meta_hashes.len().to_string()).unwrap(),
    );
    calldata.extend_from_slice(&aggregate_results.meta_hashes);

    let result = account
        .execute(vec![Call {
            to: config.contract.renewal,
            selector: selector!("batch_renew"),
            calldata,
        }])
        .send()
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let error_message = format!("An error occurred while renewing domains: {}", e);
            Err(anyhow::anyhow!(error_message))
        }
    }
}
