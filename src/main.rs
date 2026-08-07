use alloy_primitives::U256;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use celo_alloy_network::{Celo, EthereumWallet, ReceiptResponse};
use celo_alloy_rpc_types::CeloTransactionRequest;
use clap::Parser;
use eyre::{Result, bail};
use tracing::info;

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

    let signer: PrivateKeySigner = args.private_key.parse()?;
    let sender = signer.address();
    let wallet = EthereumWallet::new(signer);
    let provider = ProviderBuilder::new_with_network::<Celo>()
        .wallet(wallet)
        .connect(&args.rpc_url)
        .await?;

    let chain_id = provider.get_chain_id().await?;
    let balance = provider.get_balance(sender).await?;
    info!(%sender, chain_id, %balance, "connected");

    let request = CeloTransactionRequest::default()
        .from(sender)
        .to(sender)
        .value(U256::from(1));

    let pending = provider.send_transaction(request).await?;
    let tx_hash = *pending.tx_hash();
    info!(%tx_hash, "transaction sent");

    let receipt = pending.get_receipt().await?;
    if !receipt.status() {
        bail!(
            "transaction {tx_hash} reverted in block {:?}",
            receipt.block_number()
        );
    }
    info!(
        block = ?receipt.block_number(),
        gas_used = receipt.gas_used(),
        "transaction confirmed"
    );

    Ok(())
}
