# celox

A transaction load generator for Celo chains. Point it at an RPC endpoint and a
funded key, choose a workload, and it drives a target transaction rate from a
fleet of accounts while reporting what actually landed.

celox is the Rust successor to [tx-overload](https://github.com/celo-org/tx-overload).
It adds CIP-64 (fee-currency) transactions, real rate control, and receipt
checking — a transaction that reverts is a failure, not a success.

## Requirements

Rust, and a chain to point at. [Foundry](https://getfoundry.sh) is needed only
to run the end-to-end recipes or to recompile the Solidity contracts; the
compiled artifacts are committed, so a plain `cargo build` needs no Solidity
toolchain.

## Quick start

```sh
anvil --block-time 1 &

export CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
cargo run --release -- --tps 50 --senders 10 --duration 30s
```

The key is read from `CELOX_PRIVATE_KEY` only, never a command-line flag, so it
stays out of shell history and `ps` output.

## Workloads

| Workload   | What each transaction does                            | Knobs                     |
|------------|-------------------------------------------------------|---------------------------|
| `transfer` | 1-wei native self-transfer (the cheapest baseline)     | —                         |
| `calldata` | Random calldata to a burner account                    | `--data-size`             |
| `cip64`    | Self-transfer paying gas in a fee currency (type 0x7b) | `--fee-currency`          |
| `storage`  | Writes storage slots in one shared contract            | `--writes`, `--key-space` |

`cip64` requires `--fee-currency <address>`. The address is validated against
the chain's FeeCurrencyDirectory — resolved through the Celo registry — and its
per-currency intrinsic gas surcharge is read from there rather than guessed.
Adapter currencies (USDC, USDT) are rejected: celox cannot move them to fund
senders.

`storage` deploys `contracts/src/StateWriter.sol` and has every sender write
into it, so the load contends for one account's storage trie. `--key-space`
picks the kind of pressure: a wide range spreads writes over fresh slots and
grows the state trie, a narrow one concentrates them on hot slots that every
sender rewrites.

## How it works

**Accounts.** Sender keys are derived from the root key as
`keccak256(root_key || "celox" || index)`, so a given root always controls the
same senders and leftover balances are reused between runs. At startup celox
prints the root's balances, tops every sender up to `--fund` (and `--fund-token`
for `cip64`), and refuses to start if the root cannot cover all of it — funding
is all or nothing rather than a partial airdrop that dies halfway.

**Sending.** Each sender owns a lane: a local nonce counter and a bounded window
of in-flight transactions (`--window`). Nodes cap pending transactions per
account — 16 in reth — so a deeper window only earns rejections. A global token
bucket paces the whole fleet at `--tps`. Fee fields, gas limit and nonce are all
preset, so the usual filler round-trips per transaction disappear; the shared
fee quote refreshes every two seconds, which is what keeps transactions valid
while a saturated chain drives its base fee up.

**Results.** A status line every five seconds, then a summary with confirmed,
reverted, dropped and rejected counts plus inclusion-latency percentiles. The
exit code is zero only if every transaction sent was confirmed with status 1 —
a run that sends nothing fails too, so this works as a CI gate.

Ctrl-C stops sending and drains outstanding receipts before reporting; a second
Ctrl-C abandons them and exits immediately.

## Throughput

Offered load above what the chain can include just queues and then times out as
dropped. The ceiling is the block gas limit divided by the workload's gas cost:
anvil at 30M gas fits about 226 `storage` transactions (≈133k gas each) per
one-second block, so `--tps 300` there reports drops by design. Raise
`--senders` rather than `--window` to add capacity.

## Development

```sh
just              # format, then clippy with warnings denied
just test         # unit tests
just e2e          # transfer workload against a throwaway anvil
just e2e-storage  # storage workload; asserts the contract's slot equals the confirmed count
just e2e-cip64    # CIP-64 against a celo-reth dev node from ../celo-kona
just build-contracts   # recompile Solidity and refresh the committed artifacts
```

`e2e-cip64` needs a celo-kona checkout beside this one; anvil cannot serve
type-0x7b transactions or the fee-currency RPCs.

Design decisions and the running log live in `implementation-notes.md`.
