//! Fee quoting: one shared quote per run, refreshed in the background, so the
//! senders never pay per-transaction fee RPCs.

use std::time::Duration;

use alloy_primitives::{Address, U256};
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::Celo;
use eyre::Result;
use tokio::sync::watch;
use tracing::warn;

/// Fee suggestion shared by the senders and, as a one-shot native quote, used
/// by the funder. Denominated in the workload's fee currency when one is set,
/// otherwise in wei.
#[derive(Clone, Copy, Debug)]
pub struct FeeQuote {
    pub max_fee: u128,
    pub tip: u128,
}

/// How often the shared quote is refetched.
///
/// This has to keep up with a rising base fee, not just drift: EIP-1559 lets
/// the base fee climb 12.5% per block, so saturating a fast chain can double it
/// within a few seconds. Since the cap only leaves room for one doubling, a
/// quote older than a couple of blocks starts getting transactions rejected as
/// underpriced. One RPC every couple of seconds is a cheap price for tracking.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Fetches an initial quote and keeps it fresh. With a fee currency the quote
/// comes from Celo's parameterized gas price RPCs.
pub async fn spawn_refresher(
    provider: DynProvider<Celo>,
    fee_currency: Option<Address>,
) -> Result<watch::Receiver<FeeQuote>> {
    let (tx, rx) = watch::channel(quote(&provider, fee_currency).await?);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        tick.tick().await; // the first tick fires immediately; skip it
        loop {
            tick.tick().await;
            match quote(&provider, fee_currency).await {
                Ok(fresh) => {
                    let _ = tx.send(fresh);
                }
                Err(error) => warn!(%error, "fee refresh failed, keeping previous quote"),
            }
        }
    });
    Ok(rx)
}

async fn quote(provider: &DynProvider<Celo>, fee_currency: Option<Address>) -> Result<FeeQuote> {
    match fee_currency {
        None => native_quote(provider).await,
        Some(token) => cip64_quote(provider, token).await,
    }
}

/// Native EIP-1559 estimate; the funder uses this one-shot for its own
/// transactions, which always pay native fees.
pub async fn native_quote(provider: &DynProvider<Celo>) -> Result<FeeQuote> {
    let estimate = provider.estimate_eip1559_fees().await?;
    Ok(FeeQuote {
        max_fee: estimate.max_fee_per_gas,
        tip: estimate.max_priority_fee_per_gas,
    })
}

/// Fee-currency-denominated quote from Celo's parameterized gas price RPCs.
async fn cip64_quote(provider: &DynProvider<Celo>, token: Address) -> Result<FeeQuote> {
    let gas_price: U256 = provider
        .raw_request("eth_gasPrice".into(), (token,))
        .await?;
    let tip: U256 = provider
        .raw_request("eth_maxPriorityFeePerGas".into(), (token,))
        .await?;
    let gas_price = u128::try_from(gas_price)?;
    let tip = u128::try_from(tip)?;
    Ok(FeeQuote {
        max_fee: cap_max_fee(gas_price, tip),
        tip,
    })
}

/// celo-alloy CeloGasFiller's fee cap: room for the base fee (gas price minus
/// tip) to double while preserving the tip, falling back to `max(2·price,
/// tip)` when the node reports a tip at or above the gas price. Saturating,
/// like the upstream implementation.
fn cap_max_fee(gas_price: u128, tip: u128) -> u128 {
    if gas_price > tip {
        (gas_price - tip).saturating_mul(2).saturating_add(tip)
    } else {
        gas_price.saturating_mul(2).max(tip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_leave_room_for_base_fee_to_double() {
        // base fee 8, tip 2: cap = 2*8 + 2.
        assert_eq!(cap_max_fee(10, 2), 18);
        // Degenerate node answer, tip >= gas price: fall back to max(2*price, tip).
        assert_eq!(cap_max_fee(5, 5), 10);
        assert_eq!(cap_max_fee(3, 20), 20);
        // Zero tip: pure base-fee headroom.
        assert_eq!(cap_max_fee(7, 0), 14);
        // Saturates instead of overflowing.
        assert_eq!(cap_max_fee(u128::MAX, 1), u128::MAX);
        assert_eq!(cap_max_fee(u128::MAX, u128::MAX), u128::MAX);
    }
}
