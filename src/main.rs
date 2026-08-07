mod celo;
mod fees;
mod funder;
mod gas;
mod keys;
mod lane;
mod pacer;
mod report;
mod sender;
mod workloads;

use std::{sync::Arc, time::Duration};

use alloy_primitives::{
    Address,
    utils::{parse_ether, parse_units},
};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use celo_alloy_network::{Celo, EthereumWallet};
use clap::Parser;
use eyre::{Result, ensure};
use futures::future::join_all;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    pacer::Pacer,
    sender::Sender,
    workloads::{Workload, WorkloadKind},
};

/// Transaction load-testing tool for Celo chains.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "The root account's private key is read from CELOX_PRIVATE_KEY."
)]
struct Args {
    /// JSON-RPC endpoint of the target chain
    #[arg(long, env = "ETH_RPC_URL", default_value = "http://127.0.0.1:8545")]
    rpc_url: String,

    /// Workload to generate
    #[arg(long, value_enum, default_value_t = WorkloadKind::Transfer)]
    workload: WorkloadKind,

    /// Calldata payload size in bytes, for the `calldata` workload
    #[arg(long, default_value_t = 1024)]
    data_size: usize,

    /// Storage slots each transaction writes, for the `storage` workload
    #[arg(long, default_value_t = 5)]
    writes: u64,

    /// Slot range the `storage` workload writes into. A wide range grows the
    /// state trie; a narrow one contends on hot slots.
    #[arg(long, default_value_t = 1_000_000)]
    key_space: u64,

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

    /// Fee currency address for the `cip64` workload
    #[arg(long)]
    fee_currency: Option<Address>,

    /// Native balance each sender is topped up to at startup, in CELO
    #[arg(long, default_value = "0.1")]
    fund: String,

    /// Fee-currency balance each sender is topped up to at startup, in whole
    /// tokens (scaled by the token's own decimals)
    #[arg(long, default_value = "1")]
    fund_token: String,

    /// How long to wait for a receipt before counting a transaction as dropped
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    receipt_timeout: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // Colour only a terminal, so piped logs stay machine-readable.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "celox=info".into()),
        )
        .init();

    let args = Args::parse();
    // The upper bound keeps 1/tps a representable interval; the pacer loses
    // ticks well below it anyway (see implementation-notes.md).
    ensure!(
        args.tps > 0.0 && args.tps <= 10_000.0,
        "--tps must be between 0 and 10000"
    );
    ensure!(args.senders > 0, "--senders must be at least 1");
    ensure!(args.window > 0, "--window must be at least 1");
    // Beyond this, txpool per-tx size limits reject everything anyway.
    ensure!(
        args.data_size <= 128 * 1024,
        "--data-size must be at most 131072 bytes"
    );
    ensure!(args.writes > 0, "--writes must be at least 1");
    ensure!(args.key_space > 0, "--key-space must be at least 1");
    let target_balance = parse_ether(&args.fund)?;

    // Env only, never argv: a key on the command line leaks into shell
    // history and ps output.
    let private_key = std::env::var("CELOX_PRIVATE_KEY")
        .map_err(|_| eyre::eyre!("set CELOX_PRIVATE_KEY to the root account's private key"))?;
    let root_signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|_| eyre::eyre!("CELOX_PRIVATE_KEY is not a valid private key"))?;
    let root_address = root_signer.address();
    let sender_signers: Vec<PrivateKeySigner> = (0..args.senders)
        .map(|index| keys::derive_sender(&root_signer, index))
        .collect::<Result<_>>()?;

    // One provider (and HTTP connection pool) for the whole run: the wallet
    // holds every sender key and signs by the request's `from` field, and
    // DynProvider clones are cheap Arc handles.
    let mut wallet = EthereumWallet::new(root_signer);
    for signer in &sender_signers {
        wallet.register_signer(signer.clone());
    }
    let provider = ProviderBuilder::new_with_network::<Celo>()
        .wallet(wallet)
        .connect(&args.rpc_url)
        .await?
        .erased();
    let chain_id = provider.get_chain_id().await?;
    info!(root = %root_address, chain_id, "connected");

    // The workload's fee currency, validated against the chain's directory.
    // Resolved before funding because the funder's token leg depends on it.
    let fee_currency = match (args.workload, args.fee_currency) {
        (WorkloadKind::Cip64, Some(token)) => Some(celo::fee_currency(&provider, token).await?),
        (WorkloadKind::Cip64, None) => {
            eyre::bail!("the cip64 workload needs --fee-currency")
        }
        (_, Some(_)) => eyre::bail!("--fee-currency only applies to the cip64 workload"),
        (_, None) => None,
    };

    // The funder's own transactions always pay native fees, so it gets a
    // one-shot native quote; the senders' refreshed quote below is denominated
    // in the workload's fee currency when one is set.
    let native_fees = fees::native_quote(&provider).await?;
    let sender_addresses: Vec<_> = sender_signers.iter().map(|s| s.address()).collect();
    let token_leg = match fee_currency {
        Some(currency) => Some(funder::TokenLeg {
            token: currency.token,
            target: parse_units(&args.fund_token, currency.decimals)?.into(),
            decimals: currency.decimals,
        }),
        None => None,
    };
    funder::top_up(
        &provider,
        root_address,
        &sender_addresses,
        target_balance,
        token_leg.as_ref(),
        native_fees,
    )
    .await?;

    let fees = fees::spawn_refresher(provider.clone(), fee_currency.map(|c| c.token)).await?;

    let workload = Arc::new(
        Workload::new(
            args.workload,
            workloads::Config {
                data_size: args.data_size,
                fee_currency,
                writes: args.writes,
                key_space: args.key_space,
            },
            &provider,
        )
        .await?,
    );

    let pacer = Pacer::start(args.tps);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let reporter = report::spawn(event_rx);

    let mut tasks = Vec::with_capacity(sender_signers.len());
    for signer in sender_signers {
        let address = signer.address();
        let sender = Sender {
            provider: provider.clone(),
            address,
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

    // Every sender RPC is individually bounded, so the drain finishes within
    // receipt_timeout + RPC_TIMEOUT; the deadline is a backstop that turns a
    // wedged shutdown into a failed run instead of a hung CI job.
    let drain_deadline = args.receipt_timeout + lane::RPC_TIMEOUT + Duration::from_secs(5);
    let mut sender_failed = false;
    match tokio::time::timeout(drain_deadline, join_all(tasks)).await {
        Ok(results) => {
            for result in results {
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
        }
        Err(_elapsed) => {
            warn!("drain deadline exceeded, abandoning stuck senders");
            std::process::exit(1);
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
