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
use eyre::{Result, WrapErr, ensure};
use futures::future::join_all;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use crate::{
    pacer::Pacer,
    sender::Sender,
    workloads::{Recipient, Workload, WorkloadKind},
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
    #[arg(long, env = "CELOX_RPC_URL", default_value = "http://127.0.0.1:8545")]
    rpc_url: String,

    /// Workload to generate
    #[arg(long, value_enum, default_value_t = WorkloadKind::Transfer)]
    workload: WorkloadKind,

    /// Destination mode for the `transfer` and `cip64` workloads. `self` only
    /// burns gas; `fresh` targets a brand-new address per transaction, which
    /// for `transfer` mints an account and for `cip64` sends 1 wei of the fee
    /// currency, writing a balance slot that was zero
    #[arg(long, value_enum, default_value_t = Recipient::Sender)]
    recipient: Recipient,

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
    /// tokens (scaled by the token's own decimals). Fees are reserved from it,
    /// so it must cover `--window` transactions at the fee-currency gas price
    #[arg(long, default_value = "1")]
    fund_token: String,

    /// How long to wait for a receipt before counting a transaction as dropped
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    receipt_timeout: Duration,

    /// How often to poll for receipts. Alloy defaults to 7s for any
    /// non-localhost endpoint, which would quantise every measured latency;
    /// raise this to reduce the read load celox itself puts on the node.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "250ms")]
    poll_interval: Duration,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // Rust masks SIGPIPE, so piping celox into `head` would kill it with a
    // panic and exit 101 — neither of the codes this tool documents.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    tracing_subscriber::fmt()
        // Colour only a terminal, so piped logs stay machine-readable.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "celox=info".into()),
        )
        .init();

    match run().await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            // Just the message: eyre's default handler would add a source
            // location, which makes a user's typo look like a crash, and the
            // alternate form repeats causes that already print their own.
            eprintln!("Error: {error}");
            std::process::ExitCode::from(1)
        }
    }
}

async fn run() -> Result<u8> {
    let args = Args::parse();
    // The upper bound keeps 1/tps a representable interval; the pacer loses
    // ticks well below it anyway (see implementation-notes.md).
    ensure!(
        args.tps > 0.0 && args.tps <= 10_000.0,
        "--tps must be between 0 and 10000"
    );
    ensure!(args.senders > 0, "--senders must be at least 1");
    ensure!(args.window > 0, "--window must be at least 1");
    if args.window > lane::MAX_WINDOW {
        warn!(
            window = args.window,
            cap = lane::MAX_WINDOW,
            "--window exceeds the per-account pending limit nodes typically enforce; \
             the excess will be rejected"
        );
    }
    // Beyond this, txpool per-tx size limits reject everything anyway.
    ensure!(
        args.data_size <= 128 * 1024,
        "--data-size must be at most 131072 bytes"
    );
    // Bounded so the analytic gas limit cannot overflow; a single transaction
    // writing this many slots already exceeds any block gas limit.
    ensure!(
        args.writes > 0 && args.writes <= 10_000,
        "--writes must be between 1 and 10000"
    );
    ensure!(args.key_space > 0, "--key-space must be at least 1");
    ensure!(
        args.workload == WorkloadKind::Cip64 || args.fee_currency.is_none(),
        "--fee-currency only applies to the cip64 workload"
    );
    ensure!(
        args.workload != WorkloadKind::Cip64 || args.fee_currency.is_some(),
        "the cip64 workload needs --fee-currency"
    );
    // Parsed here rather than where it is used, so a typo fails immediately
    // instead of only on the workload that happens to read it.
    let target_balance = parse_ether(&args.fund).wrap_err("--fund is not a valid amount")?;
    // Syntax-checked here so a typo fails immediately rather than only on the
    // workload that reads it; the token's real decimals are not known yet.
    parse_units(&args.fund_token, 18).wrap_err("--fund-token is not a valid amount")?;

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
    // Alloy picks the poll interval from the URL's hostname: 250ms for
    // localhost, 7s for everything else. Left alone, a remote endpoint would
    // report latencies quantised to multiples of 7s and count landed
    // transactions as dropped.
    provider.client().set_poll_interval(args.poll_interval);
    let chain_id = provider.get_chain_id().await?;
    info!(rpc = %args.rpc_url, root = %root_address, chain_id, "connected");

    // The workload's fee currency, validated against the chain's directory.
    // Resolved before funding because the funder's token leg depends on it.
    let fee_currency = match args.fee_currency {
        Some(token) => Some(celo::fee_currency(&provider, token).await?),
        None => None,
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
    // Any contract the workload needs is deployed before funding, so the root
    // still holds the deploy cost when the solvency check runs — funding stays
    // all or nothing.
    let workload = Arc::new(
        Workload::new(
            args.workload,
            workloads::Config {
                recipient: args.recipient,
                data_size: args.data_size,
                fee_currency,
                writes: args.writes,
                key_space: args.key_space,
            },
            &provider,
        )
        .await?,
    );

    // Ask the workload for a representative transaction: if its gas limit
    // exceeds the block gas limit every send would be rejected, so fail before
    // funding rather than after paying for it.
    let workload_gas = workload
        .next(root_address)
        .inner
        .as_ref()
        .gas
        .unwrap_or_default();
    let block_gas_limit = provider
        .get_block_by_number(alloy_eips::BlockNumberOrTag::Latest)
        .await?
        .map(|block| block.header.gas_limit)
        .unwrap_or(u64::MAX);
    ensure!(
        workload_gas <= block_gas_limit,
        "this workload needs {workload_gas} gas per transaction but the chain's block gas limit is {block_gas_limit}"
    );

    // Started before funding so the funder can check the targets against the
    // fee the senders will actually pay.
    let fees = fees::spawn_refresher(provider.clone(), fee_currency.map(|c| c.token)).await?;
    let sender_max_fee = fees.borrow().max_fee;

    funder::top_up(
        &provider,
        root_address,
        &sender_addresses,
        target_balance,
        token_leg.as_ref(),
        native_fees,
        funder::window_reserve(args.window, workload_gas, sender_max_fee),
    )
    .await?;

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

    // Draining waits out the receipts of everything already sent, which takes
    // as long as the chain does. A second interrupt says the operator would
    // rather have the process back than the statistics.
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            warn!("interrupted, abandoning pending transactions");
            std::process::exit(130);
        }
    });

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
            return Ok(1);
        }
    }
    let summary = reporter.await?;
    Ok(if sender_failed {
        1
    } else {
        summary.exit_code() as u8
    })
}
