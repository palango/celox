# celox

A transaction load generator for Celo chains. Point it at an RPC endpoint and a
funded key, choose a workload, and it drives a target transaction rate from a
fleet of accounts while reporting what actually landed.

celox is the Rust successor to [tx-overload](https://github.com/celo-org/tx-overload).
It adds CIP-64 (fee-currency) transactions, real rate control, and receipt
checking — a transaction that reverts is a failure, not a success.

## Requirements

Rust, and a chain to point at. [just](https://github.com/casey/just),
[Foundry](https://getfoundry.sh) and python3 are needed only for the development
recipes; the compiled contract artifacts are committed, so a plain `cargo build`
needs no Solidity toolchain.

## Quick start

```sh
anvil --block-time 1 &

export CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
cargo run --release -- --tps 50 --senders 10 --duration 30s
```

The key is read from `CELOX_PRIVATE_KEY` only, never a command-line flag, so it
stays out of shell history and `ps` output. The endpoint comes from `--rpc-url`
or `CELOX_RPC_URL`; celox deliberately ignores Foundry's `ETH_RPC_URL` so an
exported variable cannot silently point a load test at a live chain. Every run
logs the endpoint and chain id it connected to — check them.

Exit codes: `0` clean, `1` any failure (including a run that sent nothing, or a
shutdown that could not drain), `130` interrupted.

## Docker

```sh
docker build -t celox .
docker run --rm -e CELOX_PRIVATE_KEY \
    celox --rpc-url https://forno.celo-sepolia.celo-testnet.org \
          --tps 20 --senders 10 --duration 30s
```

The image carries only the binary and a trust store; the build needs no Solidity
toolchain because the contract artifact is committed. To reach a node on the
host — anvil, say — use `--network host` on Linux, or
`--rpc-url http://host.docker.internal:8545` on macOS and Windows.

## Workloads

Each workload stresses a different part of a node. All of them share the pacing,
funding and reporting described below; they differ only in what transaction they
produce. Every example assumes `CELOX_PRIVATE_KEY` is exported and a chain is
reachable at `--rpc-url` (default `http://127.0.0.1:8545`).

### `transfer` — baseline throughput and account-trie growth

A 1-wei native transfer: 21,000 gas, no calldata, no contract. `--recipient`
decides what state pressure that puts on the chain:

```sh
# Baseline (default): self-transfers, senders only burn gas
celox --workload transfer --tps 200 --senders 20 --duration 1m

# Account growth: every transfer mints a brand-new account
celox --workload transfer --recipient fresh --tps 200 --senders 20
```

`self` touches no account but the sender's own, so it measures how many
transactions per second the pipeline and the node can move — use it to find a
node's transaction-count ceiling, and as the control when a heavier workload
behaves oddly.

`fresh` sends the wei to a random never-seen address instead, so the chain
inserts one new account-trie leaf per transaction, at keccak-uniform positions
with no path locality — the load shape a burst of new accounts puts on a
node's state-root computation. Account creation costs nothing at the
transaction level, so gas stays exactly 21,000 and this is the cheapest
possible way to grow the account trie: new accounts per second equals the
confirmed rate. (For growing a contract's *storage* trie, see `storage`
below.) Each transaction strands 1 wei on a keyless address, which is
negligible next to its gas.

### `calldata` — data throughput

Random bytes sent to a burner account, so the transaction pays for its payload
and runs no code. Gas is the EIP-7623 calldata floor, priced exactly.

```sh
celox --workload calldata --data-size 4096 --tps 50 --senders 20 --duration 1m
```

`--data-size` is the payload in bytes (default 1024, max 128 KiB). Raise it to
push bandwidth, block-size limits and the propagation path rather than execution.

### `cip64` — fee-currency transactions

A self-transfer whose gas is paid in an ERC-20 rather than CELO, as a type-`0x7b`
transaction. This is the workload that exercises Celo's alternative-fee path end
to end: the directory lookup, the debit before execution and the credit after.

```sh
# Celo Sepolia, paying fees in cUSD
celox --rpc-url https://forno.celo-sepolia.celo-testnet.org       --workload cip64       --fee-currency 0xEF4d55D6dE8e8d73232827Cd1e9b2F2dBb45bC80       --fund-token 5 --tps 20 --senders 10 --duration 1m
```

The address is validated against the chain's FeeCurrencyDirectory — resolved
through the Celo registry — and the per-currency intrinsic gas surcharge is read
from there rather than guessed. `--fund-token` is how much of the token each
sender is given, in whole tokens scaled by the token's own decimals; the root
account must hold enough for every sender.

Adapter currencies (USDC, USDT) are rejected up front: they exist to present a
6-decimal token as an 18-decimal fee currency, revert on `decimals()`, and cannot
be transferred, so celox cannot fund senders with them.

Other Celo Sepolia currencies: cEUR `0x6B172e333e2978484261D7eCC3DE491E79764BbC`,
cREAL `0x13d68A1Bf4a8cB7d9feF54EF70401871b666269c`.

### `storage` — state growth and contention

Deploys `contracts/src/StateWriter.sol` and has every sender call it, so the load
converges on one contract's storage trie. `--writes` sets how many slots each
transaction touches; `--key-space` decides what kind of pressure that is.

```sh
# State growth: writes land on fresh slots and the trie keeps growing
celox --workload storage --writes 10 --key-space 100000000 --tps 50 --senders 20

# Contention: every sender rewrites the same handful of hot slots
celox --workload storage --writes 10 --key-space 16 --tps 50 --senders 20
```

A fresh slot costs about 22,100 gas and a rewritten one about 5,000, so the two
modes differ in cost as well as in what they stress: the first grows state and
the work of computing a state root, the second concentrates writes where
execution must serialise them. Gas is preset as an upper bound for the fresh
case, which over-provisions by a few percent once a key range has filled.

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
reverted, dropped and rejected counts plus latency percentiles. The exit code is
zero only if every transaction sent was confirmed with status 1 — a run that
sends nothing fails too, so this works as a CI gate.

The reported rate divides confirmations by the sending window, not by the run
including its drain. The percentiles cover confirmed transactions only: anything
counted `dropped` waited the full `--receipt-timeout` by definition, so read the
percentiles alongside the drop count rather than on their own.

Ctrl-C stops sending and drains outstanding receipts before reporting; pressing
it again abandons them and exits 130. Once a `--duration` run has finished
sending, it is already draining, so a single Ctrl-C abandons.

## Throughput

Offered load above what the chain can include just queues and then times out as
dropped. The ceiling is the block gas limit divided by the workload's gas cost:
anvil at 30M gas fits about 226 `storage` transactions (≈133k gas each) per
one-second block, so `--tps 300` there reports drops by design. The cheap
`transfer` workload reaches roughly 1400 tps on the same node. Raise `--senders`
rather than `--window` to add capacity — nodes cap pending transactions per
account.

Two settings matter away from localhost. `--fund` defaults to 0.1 CELO, which
suits a dev chain with a negligible base fee; a chain with a real floor (Celo
mainnet's is 200 gwei) needs one to two orders of magnitude more, because a node
reserves `gas_limit × max_fee` for every pending transaction and celox will warn
if the target cannot cover a full window. `--poll-interval` defaults to 250ms;
alloy would otherwise poll a remote endpoint every 7 seconds and quantise every
measured latency to that. Note that polling is celox's own read load on the node
under test — roughly one `eth_getTransactionReceipt` per in-flight transaction
per interval — so raise it when measuring a node whose read path you care about.

## Development

```sh
just              # format, then clippy with warnings denied
just test         # unit tests
just e2e          # transfer workload against a throwaway anvil
just e2e-storage  # storage workload; asserts the contract's slot equals the confirmed count
just e2e-cip64    # CIP-64 against a celo-reth dev node from ../celo-kona
just build-contracts   # recompile Solidity and refresh the committed artifacts
just docker-build      # build the container image
```

`e2e-cip64` needs a celo-kona checkout beside this one; anvil cannot serve
type-0x7b transactions or the fee-currency RPCs.

Design decisions and the running log live in `implementation-notes.md`.
