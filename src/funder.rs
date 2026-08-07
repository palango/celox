use std::{future::IntoFuture, time::Duration};

use alloy_primitives::{
    Address, U256,
    utils::{format_ether, format_units},
};
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Result, bail, ensure};
use futures::future::try_join_all;
use tracing::{info, warn};

use crate::{
    celo,
    fees::FeeQuote,
    gas,
    lane::{Lane, MAX_WINDOW, Submitted},
};

/// Upper bound for one ERC-20 `transfer`, not an exact price: the cost depends
/// on whether the recipient's balance slot is fresh. Unused gas is refunded.
const ERC20_TRANSFER_GAS: u64 = 100_000;

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
    window: usize,
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

    let native_topups = native_shortfalls(provider, senders, native_target).await?;
    let token_topups = match token_leg {
        Some(leg) => token_shortfalls(provider, senders, leg).await?,
        None => Vec::new(),
    };

    // Solvency for both legs before the first send: funding must be all or
    // nothing, not a partial airdrop that dies halfway.
    let native_total: U256 = native_topups.iter().map(|(_, amount)| *amount).sum();
    let funding_gas = U256::from(native_topups.len() as u64 * gas::TRANSFER)
        + U256::from(token_topups.len() as u64 * ERC20_TRANSFER_GAS);
    let native_needed = native_total + funding_gas * U256::from(fees.max_fee);
    ensure!(
        root_native >= native_needed,
        "root {root} holds {} CELO but funding all {} senders needs {} CELO (incl. gas)",
        format_ether(root_native),
        senders.len(),
        format_ether(native_needed),
    );
    if let (Some(leg), Some(balance)) = (token_leg, root_token) {
        let token_total: U256 = token_topups.iter().map(|(_, amount)| *amount).sum();
        ensure!(
            balance >= token_total,
            "root {root} holds {} of fee currency {} but funding all {} senders needs {}",
            format_units(balance, leg.decimals)?,
            leg.token,
            senders.len(),
            format_units(token_total, leg.decimals)?,
        );
    }

    // A node reserves gas_limit * max_fee for every pending transaction, so a
    // sender whose balance cannot cover its whole window has the tail rejected
    // as underfunded. gas::TRANSFER is the floor — every workload costs at
    // least that — so this is a lower bound on what a sender needs.
    let window_reserve =
        U256::from(window as u64 * gas::TRANSFER).saturating_mul(U256::from(fees.max_fee));
    if native_target < window_reserve {
        warn!(
            fund = %format_ether(native_target),
            needed = %format_ether(window_reserve),
            "--fund is below what a node reserves for a full window at the current fee; \
             expect insufficient-funds rejections"
        );
    }

    if native_topups.is_empty() && token_topups.is_empty() {
        info!("all senders already funded");
        return Ok(());
    }

    let mut requests = Vec::with_capacity(native_topups.len() + token_topups.len());
    for (address, amount) in &native_topups {
        requests.push(
            CeloTransactionRequest::default()
                .to(*address)
                .value(*amount)
                .gas_limit(gas::TRANSFER),
        );
    }
    if let Some(leg) = token_leg {
        for (address, amount) in &token_topups {
            requests.push(
                celo::erc20_transfer(provider, leg.token, *address, *amount)
                    .gas_limit(ERC20_TRANSFER_GAS),
            );
        }
    }

    // The funder is a single account (root), so one lane bounds its in-flight
    // funding to the per-account pool cap; the loop keeps it full and awaits
    // each receipt, failing the run if any funding transaction doesn't land.
    let mut lane = Lane::open(provider.clone(), root, MAX_WINDOW, Duration::from_secs(30)).await?;
    let mut requests = requests.into_iter();
    let mut outstanding = 0usize;
    loop {
        while lane.has_capacity() {
            let Some(request) = requests.next() else {
                break;
            };
            match lane.submit(request, fees).await {
                Submitted::Sent => outstanding += 1,
                Submitted::Rejected => bail!("a funding transaction was rejected"),
            }
        }
        if outstanding == 0 {
            break;
        }
        let (_, status) = lane.next_completed().await;
        outstanding -= 1;
        match status {
            Some(true) => {}
            Some(false) => bail!("a funding transaction reverted"),
            None => bail!("timed out waiting for a funding receipt"),
        }
    }
    // A receipt with status 1 does not prove the tokens arrived: an ERC-20 may
    // return false, or skim a transfer fee, without reverting. Confirm against
    // balances before a run depends on them.
    if let Some(leg) = token_leg {
        let still_short = token_shortfalls(provider, senders, leg).await?;
        ensure!(
            still_short.is_empty(),
            "{} senders are still below the fee-currency target after funding; \
             is {} a standard ERC-20?",
            still_short.len(),
            leg.token
        );
    }
    info!(
        native = native_topups.len(),
        token = token_topups.len(),
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
