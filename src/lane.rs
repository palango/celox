//! One account's transaction pipeline: a local nonce counter plus a bounded
//! window of in-flight transactions. Both the senders (one lane each, driven by
//! a pacer) and the funder (one lane for the root, drained to completion) push
//! requests through a lane, so nonce assignment, submission, the pool-cap
//! window and receipt waiting live in one place.

use std::{
    future::IntoFuture,
    time::{Duration, Instant},
};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider};
use celo_alloy_network::{Celo, ReceiptResponse};
use celo_alloy_rpc_types::CeloTransactionRequest;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesOrdered};
use tracing::{debug, warn};

use crate::fees::FeeQuote;

/// Bound on every submission and nonce RPC. Without it a connection that
/// accepts a request but never answers blocks the caller's loop where a stop
/// signal cannot reach it. Public because main's drain deadline must exceed
/// `receipt_timeout + RPC_TIMEOUT`, the worst legitimate drain.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Nodes cap pending transactions per account (reth: 16), so a lane's window
/// cannot exceed it; this is the funder's default and the senders' upper bound.
pub const MAX_WINDOW: usize = 16;

/// Receipt wait result: latency plus `Some(status)`, or `None` when no receipt
/// arrived within the timeout.
type ReceiptWait = BoxFuture<'static, (Duration, Option<bool>)>;

/// Outcome of a submission attempt. A rejection means the node refused the
/// transaction or never answered — in both cases the local nonce may now be
/// wrong, so the caller should drain and resync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Submitted {
    Sent,
    Rejected,
}

pub struct Lane {
    provider: DynProvider<Celo>,
    account: Address,
    nonce: u64,
    window: usize,
    receipt_timeout: Duration,
    in_flight: FuturesOrdered<ReceiptWait>,
}

impl Lane {
    /// Opens a lane for `account`, fetching its pending nonce.
    pub async fn open(
        provider: DynProvider<Celo>,
        account: Address,
        window: usize,
        receipt_timeout: Duration,
    ) -> eyre::Result<Self> {
        let nonce = pending_nonce(&provider, account).await?;
        Ok(Self {
            provider,
            account,
            nonce,
            window,
            receipt_timeout,
            in_flight: FuturesOrdered::new(),
        })
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    pub fn has_capacity(&self) -> bool {
        self.in_flight.len() < self.window
    }

    /// Refetches the on-chain nonce after a rejection or drop left the local
    /// counter suspect. Only meaningful once the window has drained.
    pub async fn resync(&mut self) -> eyre::Result<()> {
        self.nonce = pending_nonce(&self.provider, self.account).await?;
        debug!(account = %self.account, nonce = self.nonce, "nonce resynced");
        Ok(())
    }

    /// Fills the account fields on `request`, submits it, and on success queues
    /// its receipt wait and advances the nonce.
    pub async fn submit(&mut self, request: CeloTransactionRequest, fees: FeeQuote) -> Submitted {
        // Chain id is left to the provider's ChainIdFiller, which caches it
        // after one fetch; everything else is preset so no filler does RPC.
        let request = request
            .from(self.account)
            .nonce(self.nonce)
            .max_fee_per_gas(fees.max_fee)
            .max_priority_fee_per_gas(fees.tip);

        let started = Instant::now();
        let sent = tokio::time::timeout(RPC_TIMEOUT, self.provider.send_transaction(request)).await;
        let pending = match sent {
            Ok(Ok(pending)) => pending,
            Ok(Err(error)) => {
                warn!(account = %self.account, %error, "transaction rejected");
                return Submitted::Rejected;
            }
            Err(_elapsed) => {
                warn!(account = %self.account, "submission timed out");
                return Submitted::Rejected;
            }
        };
        self.nonce += 1;

        let receipt_timeout = self.receipt_timeout;
        self.in_flight.push_back(
            async move {
                let status =
                    match tokio::time::timeout(receipt_timeout, pending.get_receipt()).await {
                        Ok(Ok(receipt)) => Some(receipt.status()),
                        Ok(Err(error)) => {
                            debug!(%error, "receipt fetch failed");
                            None
                        }
                        Err(_elapsed) => None,
                    };
                (started.elapsed(), status)
            }
            .boxed(),
        );
        Submitted::Sent
    }

    /// Awaits the next completed transaction. Stays pending forever when the
    /// window is empty, so it is safe to select on without busy-looping; it is
    /// cancel-safe, so a lost `select!` race drops no queued receipt.
    pub async fn next_completed(&mut self) -> (Duration, Option<bool>) {
        match self.in_flight.next().await {
            Some(outcome) => outcome,
            None => std::future::pending().await,
        }
    }
}

async fn pending_nonce(provider: &DynProvider<Celo>, account: Address) -> eyre::Result<u64> {
    let fetch = provider.get_transaction_count(account).pending();
    Ok(tokio::time::timeout(RPC_TIMEOUT, fetch.into_future()).await??)
}
