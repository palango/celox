use std::{future::IntoFuture, time::Duration};

use alloy_primitives::{
    Address, U256,
    utils::{format_ether, format_units},
};
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::{Celo, ReceiptResponse};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, WrapErr, ensure};
use futures::future::try_join_all;
use tracing::info;

use crate::{celo, fees::FeeQuote, gas};

/// Upper bound for one ERC-20 `transfer`, not an exact price: the cost depends
/// on whether the recipient's balance slot is fresh. Unused gas is refunded.
const ERC20_TRANSFER_GAS: u64 = 100_000;

/// How many funding transactions go out before awaiting their receipts. Nodes
/// cap pending transactions per account (reth: 16), so the root cannot have a
/// whole large fleet's funding in flight at once.
const FUNDING_BATCH: usize = 10;

/// The fee-currency leg of funding: senders paying gas in `token` get topped
/// up to `target` of it. `decimals` is only for display.
pub struct TokenLeg {
    pub token: Address,
    pub target: U256,
    pub decimals: u8,
}

/// Tops every sender below its target back up to it from the root account —
/// native CELO always, plus the fee-currency token when the workload pays gas
/// in one. Root balances are shown first and both legs are checked for
/// solvency before anything is sent.
///
/// All top-ups go out with consecutive preset nonces before any receipt is
/// awaited, so funding N senders costs about one block, not N.
pub async fn top_up(
    provider: &DynProvider<Celo>,
    root: Address,
    senders: &[Address],
    native_target: U256,
    token_leg: Option<&TokenLeg>,
    fees: FeeQuote,
) -> Result<()> {
    let root_native = provider.get_balance(root).await?;
    let root_token = match token_leg {
        Some(leg) => Some(celo::erc20_balance(provider, leg.token, root).await?),
        None => None,
    };
    match (token_leg, root_token) {
        (Some(leg), Some(balance)) => info!(
            celo = %format_ether(root_native),
            token = %format_units(balance, leg.decimals)?,
            token_address = %leg.token,
            "root balances"
        ),
        _ => info!(celo = %format_ether(root_native), "root balance"),
    }

    let native_shortfalls = native_shortfalls(provider, senders, native_target).await?;
    let token_shortfalls = match token_leg {
        Some(leg) => token_shortfalls(provider, senders, leg).await?,
        None => Vec::new(),
    };

    // Solvency for both legs before the first send: funding must be all or
    // nothing, not a partial airdrop that dies halfway.
    let native_total: U256 = native_shortfalls.iter().map(|(_, amount)| *amount).sum();
    let funding_gas = U256::from(native_shortfalls.len() as u64 * gas::TRANSFER)
        + U256::from(token_shortfalls.len() as u64 * ERC20_TRANSFER_GAS);
    let native_needed = native_total + funding_gas * U256::from(fees.max_fee);
    ensure!(
        root_native >= native_needed,
        "root {root} holds {} CELO but funding all {} senders needs {} CELO (incl. gas)",
        format_ether(root_native),
        senders.len(),
        format_ether(native_needed),
    );
    if let (Some(leg), Some(balance)) = (token_leg, root_token) {
        let token_total: U256 = token_shortfalls.iter().map(|(_, amount)| *amount).sum();
        ensure!(
            balance >= token_total,
            "root {root} holds {} of fee currency {} but funding all {} senders needs {}",
            format_units(balance, leg.decimals)?,
            leg.token,
            senders.len(),
            format_units(token_total, leg.decimals)?,
        );
    }

    if native_shortfalls.is_empty() && token_shortfalls.is_empty() {
        info!("all senders already funded");
        return Ok(());
    }

    let mut requests = Vec::with_capacity(native_shortfalls.len() + token_shortfalls.len());
    for (address, amount) in &native_shortfalls {
        requests.push(
            CeloTransactionRequest::default()
                .to(*address)
                .value(*amount)
                .gas_limit(gas::TRANSFER),
        );
    }
    if let Some(leg) = token_leg {
        for (address, amount) in &token_shortfalls {
            requests.push(
                celo::erc20_transfer(provider, leg.token, *address, *amount)
                    .gas_limit(ERC20_TRANSFER_GAS),
            );
        }
    }

    let mut nonce = provider.get_transaction_count(root).pending().await?;
    for batch in requests.chunks(FUNDING_BATCH) {
        let mut pending = Vec::with_capacity(batch.len());
        for request in batch {
            pending.push(send(provider, request.clone(), root, &mut nonce, fees).await?);
        }
        for wait in pending {
            let receipt = tokio::time::timeout(Duration::from_secs(30), wait.get_receipt())
                .await
                .wrap_err("timed out waiting for a funding receipt")??;
            ensure!(
                receipt.status(),
                "funding transaction {} reverted",
                receipt.transaction_hash()
            );
        }
    }
    info!(
        native = native_shortfalls.len(),
        token = token_shortfalls.len(),
        "senders funded"
    );
    Ok(())
}

async fn native_shortfalls(
    provider: &DynProvider<Celo>,
    senders: &[Address],
    target: U256,
) -> Result<Vec<(Address, U256)>> {
    let balances = try_join_all(
        senders
            .iter()
            .map(|address| provider.get_balance(*address).into_future()),
    )
    .await?;
    Ok(below_target(senders, balances, target))
}

async fn token_shortfalls(
    provider: &DynProvider<Celo>,
    senders: &[Address],
    leg: &TokenLeg,
) -> Result<Vec<(Address, U256)>> {
    let balances = try_join_all(
        senders
            .iter()
            .map(|address| celo::erc20_balance(provider, leg.token, *address)),
    )
    .await?;
    Ok(below_target(senders, balances, leg.target))
}

fn below_target(senders: &[Address], balances: Vec<U256>, target: U256) -> Vec<(Address, U256)> {
    senders
        .iter()
        .zip(balances)
        .filter(|(_, balance)| *balance < target)
        .map(|(address, balance)| (*address, target - balance))
        .collect()
}

async fn send(
    provider: &DynProvider<Celo>,
    request: CeloTransactionRequest,
    root: Address,
    nonce: &mut u64,
    fees: FeeQuote,
) -> Result<alloy_provider::PendingTransactionBuilder<Celo>> {
    // Chain id is left to the provider's cached ChainIdFiller.
    let request = request
        .from(root)
        .nonce(*nonce)
        .max_fee_per_gas(fees.max_fee)
        .max_priority_fee_per_gas(fees.tip);
    *nonce += 1;
    Ok(provider.send_transaction(request).await?)
}
