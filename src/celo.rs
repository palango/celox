//! Celo chain plumbing: registry resolution, the fee currency directory, and
//! the ERC-20 calls the funder needs.

use alloy_primitives::{Address, U256, address};
use alloy_provider::{DynProvider, Provider};
use alloy_sol_types::sol;
use celo_alloy_network::Celo;
use celo_alloy_rpc_types::CeloTransactionRequest;
use eyre::{Context, Result, bail};
use tracing::{info, warn};

/// The Celo registry proxy; the same address on every Celo chain.
pub const REGISTRY: Address = address!("000000000000000000000000000000000000ce10");

/// Mainnet's FeeCurrencyDirectory. celo-reth dev chains deploy the directory
/// at this address without a registry contract, so it doubles as the fallback
/// when `REGISTRY` has no code.
const MAINNET_FEE_CURRENCY_DIRECTORY: Address =
    address!("15F344b9E6c3Cb6F0376A36A64928b13F62C6276");

sol! {
    #[sol(rpc)]
    interface IRegistry {
        function getAddressForStringOrDie(string identifier) external view returns (address addr);
    }

    #[sol(rpc)]
    interface IFeeCurrencyDirectory {
        struct CurrencyConfig { address oracle; uint256 intrinsicGas; }
        function getCurrencyConfig(address token) external view returns (CurrencyConfig config);
    }

    #[sol(rpc)]
    interface IErc20 {
        function balanceOf(address owner) external view returns (uint256 balance);
        function transfer(address to, uint256 value) external returns (bool success);
        function decimals() external view returns (uint8);
    }
}

/// A fee currency as the chain knows it: the address that goes into a
/// transaction's `feeCurrency` field, its intrinsic gas surcharge from the
/// directory, and the token's own decimals.
#[derive(Clone, Copy, Debug)]
pub struct FeeCurrency {
    pub token: Address,
    pub intrinsic_gas: u64,
    pub decimals: u8,
}

/// Validates `token` against the chain's FeeCurrencyDirectory and reads its
/// intrinsic gas surcharge.
pub async fn fee_currency(provider: &DynProvider<Celo>, token: Address) -> Result<FeeCurrency> {
    let directory = resolve_directory(provider).await?;
    let config = IFeeCurrencyDirectory::new(directory, provider)
        .getCurrencyConfig(token)
        .call()
        .await
        .wrap_err("getCurrencyConfig failed — is this a Celo chain?")?;
    if config.oracle == Address::ZERO {
        bail!("{token} is not a registered fee currency on this chain");
    }

    // Tokens whose decimals don't match gas accounting (USDC and USDT, both
    // 6-decimal) are registered through adapter contracts, which revert on
    // decimals() and have no transfer() — the funder cannot move them.
    let decimals = IErc20::new(token, provider)
        .decimals()
        .call()
        .await
        .map_err(|_| {
            eyre::eyre!(
                "{token} looks like a fee-currency adapter (decimals() reverts); \
                 funding through adapters is unsupported — use a plain token currency"
            )
        })?;

    // A directory entry is chain-supplied: convert fallibly so a malformed or
    // foreign contract produces celox's own error, not a ruint panic.
    let intrinsic_gas = u64::try_from(config.intrinsicGas).map_err(|_| {
        eyre::eyre!(
            "fee currency {token} reports an unusable intrinsic gas of {}",
            config.intrinsicGas
        )
    })?;
    let fee_currency = FeeCurrency {
        token,
        intrinsic_gas,
        decimals,
    };
    info!(
        token = %token,
        intrinsic_gas = fee_currency.intrinsic_gas,
        decimals,
        %directory,
        "fee currency resolved"
    );
    Ok(fee_currency)
}

/// Finds the FeeCurrencyDirectory through the registry, falling back to the
/// mainnet address on chains without a registry contract (dev chains).
async fn resolve_directory(provider: &DynProvider<Celo>) -> Result<Address> {
    let code = provider.get_code_at(REGISTRY).await?;
    if code.is_empty() {
        warn!(
            directory = %MAINNET_FEE_CURRENCY_DIRECTORY,
            "no registry contract on this chain, assuming the directory at mainnet's address"
        );
        return Ok(MAINNET_FEE_CURRENCY_DIRECTORY);
    }
    Ok(IRegistry::new(REGISTRY, provider)
        .getAddressForStringOrDie("FeeCurrencyDirectory".into())
        .call()
        .await?)
}

pub async fn erc20_balance(
    provider: &DynProvider<Celo>,
    token: Address,
    owner: Address,
) -> Result<U256> {
    Ok(IErc20::new(token, provider).balanceOf(owner).call().await?)
}

/// An unsigned `transfer(to, value)` call on `token`; the funder fills in the
/// account fields.
pub fn erc20_transfer(
    provider: &DynProvider<Celo>,
    token: Address,
    to: Address,
    value: U256,
) -> CeloTransactionRequest {
    IErc20::new(token, provider)
        .transfer(to, value)
        .into_transaction_request()
}
