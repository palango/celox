mod funder;
mod keys;
mod pacer;
mod report;
mod sender;
mod workloads;

use std::{sync::Arc, time::Duration};

use alloy_primitives::utils::parse_ether;
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use celo_alloy_network::{Celo, EthereumWallet};
use clap::Parser;
use eyre::{Result, ensure};
use futures::future::join_all;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    pacer::Pacer,
    sender::{FeeQuote, Sender},
    workloads::{Workload, WorkloadKind},
};

/// Transaction load-testing tool for Celo chains.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// JSON-RPC endpoint of the target chain
    #[arg(long, env = "ETH_RPC_URL", default_value = "http://127.0.0.1:8545")]
    rpc_url: String,

    /// Private key of the funded root account
    #[arg(long, env = "CELOX_PRIVATE_KEY", hide_env_values = true)]
    private_key: String,

    /// Workload to generate
    #[arg(long, value_enum, default_value_t = WorkloadKind::Transfer)]
    workload: WorkloadKind,

    /// Target transactions per second across all senders
    #[arg(long, default_value_t = 10.0)]
    tps: f64,

    /// Number of sender accounts, derived deterministically from the root key
    #[arg(long, default_value_t = 10)]
    senders: u32,

    /// Maximum transactions in flight per sender
    #[arg(long, default_value_t = 10)]
    window: usize,

    /// How long to spam; runs until ctrl-c when absent
    #[arg(long, value_parser = humantime::parse_duration)]
    duration: Option<Duration>,

    /// Native balance each sender is topped up to at startup, in CELO
    #[arg(long, default_value = "0.1")]
    fund: String,

    /// How long to wait for a receipt before counting a transaction as dropped
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    receipt_timeout: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "celox=info".into()),
        )
        .init();

    let args = Args::parse();
    ensure!(args.tps > 0.0, "--tps must be positive");
    ensure!(args.senders > 0, "--senders must be at least 1");
    ensure!(args.window > 0, "--window must be at least 1");
    let target_balance = parse_ether(&args.fund)?;

    let root_signer: PrivateKeySigner = args.private_key.parse()?;
    let root_address = root_signer.address();
    let root = connect(&args.rpc_url, root_signer.clone()).await?;
    let chain_id = root.get_chain_id().await?;
    info!(root = %root_address, chain_id, "connected");

    let fees = spawn_fee_refresher(root.clone()).await?;

    let sender_signers: Vec<PrivateKeySigner> = (0..args.senders)
        .map(|index| keys::derive_sender(&root_signer, index))
        .collect::<Result<_>>()?;
    let sender_addresses: Vec<_> = sender_signers.iter().map(|s| s.address()).collect();
    funder::top_up(
        &root,
        root_address,
        &sender_addresses,
        target_balance,
        *fees.borrow(),
        chain_id,
    )
    .await?;

    let mut workload = Workload::new(args.workload);
    workload.setup(&root).await?;
    let workload = Arc::new(workload);

    let pacer = Pacer::start(args.tps);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let reporter = report::spawn(event_rx);

    let mut tasks = Vec::with_capacity(sender_signers.len());
    for signer in sender_signers {
        let address = signer.address();
        let sender = Sender {
            provider: connect(&args.rpc_url, signer).await?,
            address,
            chain_id,
            window: args.window,
            receipt_timeout: args.receipt_timeout,
            workload: Arc::clone(&workload),
            pacer: Arc::clone(&pacer),
            fees: fees.clone(),
            events: event_tx.clone(),
            stop: stop_rx.clone(),
        };
        tasks.push(sender.spawn());
    }
    drop(event_tx);
    info!(
        senders = tasks.len(),
        tps = args.tps,
        window = args.window,
        "spamming"
    );

    match args.duration {
        Some(duration) => {
            tokio::select! {
                _ = tokio::time::sleep(duration) => info!("duration elapsed, draining"),
                _ = tokio::signal::ctrl_c() => info!("interrupted, draining"),
            }
        }
        None => {
            let _ = tokio::signal::ctrl_c().await;
            info!("interrupted, draining");
        }
    }
    // Cannot fail while main's own stop_rx binding is alive, but never let a
    // refactor of that binding skip the summary.
    let _ = stop_tx.send(true);

    let mut sender_failed = false;
    for result in join_all(tasks).await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(%error, "sender failed");
                sender_failed = true;
            }
            Err(error) => {
                warn!(%error, "sender task panicked");
                sender_failed = true;
            }
        }
    }
    let summary = reporter.await?;
    let exit_code = if sender_failed {
        1
    } else {
        summary.exit_code()
    };
    std::process::exit(exit_code);
}

async fn connect(rpc_url: &str, signer: PrivateKeySigner) -> Result<DynProvider<Celo>> {
    let provider = ProviderBuilder::new_with_network::<Celo>()
        .wallet(EthereumWallet::new(signer))
        .connect(rpc_url)
        .await?;
    Ok(provider.erased())
}

/// Fetches an initial EIP-1559 fee estimate and keeps refreshing it every 30s.
async fn spawn_fee_refresher(provider: DynProvider<Celo>) -> Result<watch::Receiver<FeeQuote>> {
    let estimate = provider.estimate_eip1559_fees().await?;
    let (tx, rx) = watch::channel(FeeQuote {
        max_fee: estimate.max_fee_per_gas,
        tip: estimate.max_priority_fee_per_gas,
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.tick().await; // the first tick fires immediately; skip it
        loop {
            tick.tick().await;
            match provider.estimate_eip1559_fees().await {
                Ok(estimate) => {
                    let _ = tx.send(FeeQuote {
                        max_fee: estimate.max_fee_per_gas,
                        tip: estimate.max_priority_fee_per_gas,
                    });
                }
                Err(error) => warn!(%error, "fee refresh failed, keeping previous quote"),
            }
        }
    });
    Ok(rx)
}
