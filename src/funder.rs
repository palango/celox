use std::{future::IntoFuture, time::Duration};

use alloy_primitives::{Address, U256};
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::{Celo, ReceiptResponse, TransactionBuilder};
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, ensure};
use futures::future::try_join_all;
use tracing::info;

use crate::sender::FeeQuote;

/// Tops every sender below `target_balance` back up to it from the root account.
///
/// All top-ups go out with consecutive preset nonces before any receipt is
/// awaited, so funding N senders costs about one block, not N.
pub async fn top_up(
    provider: &DynProvider<Celo>,
    root: Address,
    senders: &[Address],
    target_balance: U256,
    fees: FeeQuote,
    chain_id: u64,
) -> Result<()> {
    let balances = try_join_all(
        senders
            .iter()
            .map(|address| provider.get_balance(*address).into_future()),
    )
    .await?;
    let shortfalls: Vec<(Address, U256)> = senders
        .iter()
        .zip(balances)
        .filter(|(_, balance)| *balance < target_balance)
        .map(|(address, balance)| (*address, target_balance - balance))
        .collect();
    if shortfalls.is_empty() {
        info!("all senders already funded");
        return Ok(());
    }

    let total: U256 = shortfalls.iter().map(|(_, amount)| *amount).sum();
    let gas: U256 = U256::from(shortfalls.len()) * U256::from(21_000u64) * U256::from(fees.max_fee);
    let needed = total + gas;
    let root_balance = provider.get_balance(root).await?;
    ensure!(
        root_balance >= needed,
        "root {root} holds {root_balance} wei but funding needs {needed} wei (incl. gas)"
    );

    let mut nonce = provider.get_transaction_count(root).pending().await?;
    let mut pending = Vec::with_capacity(shortfalls.len());
    for (address, amount) in &shortfalls {
        let request = CeloTransactionRequest::default()
            .from(root)
            .to(*address)
            .value(*amount)
            .gas_limit(21_000)
            .nonce(nonce)
            .max_fee_per_gas(fees.max_fee)
            .max_priority_fee_per_gas(fees.tip)
            .with_chain_id(chain_id);
        pending.push(provider.send_transaction(request).await?);
        nonce += 1;
    }
    for wait in pending {
        let receipt = tokio::time::timeout(Duration::from_secs(30), wait.get_receipt()).await??;
        ensure!(
            receipt.status(),
            "funding transaction {} reverted",
            receipt.transaction_hash()
        );
    }
    info!(topped_up = shortfalls.len(), %total, "senders funded");
    Ok(())
}
