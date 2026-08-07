# celox

A transaction load generator for Celo chains, in Rust. `README.md` is the user-facing
guide; `implementation-notes.md` is the running log of decisions, deviations and
verification evidence — read it before resuming work, and append to it when an edge case
forces a deviation.

## Commands

```sh
just                   # format + clippy with warnings denied — run before every commit
just test              # unit tests
just e2e               # transfer workload against a throwaway anvil
just e2e-storage       # storage workload; asserts the contract slot equals the confirmed count
just e2e-cip64         # CIP-64 against a celo-reth dev node built from ../celo-kona
just build-contracts   # recompile Solidity, refresh the committed artifacts
```

Never report work done without running the check that covers it. A change to sending,
funding or fee logic needs an e2e run, not just unit tests.

## Layout

| Module | Owns |
|---|---|
| `main.rs` | CLI, validation, wiring, shutdown. Orchestration only. |
| `lane.rs` | One account's pipeline: nonce, bounded in-flight window, submission, receipts. |
| `sender.rs` | One spamming account: paced `select!` loop driving a lane. |
| `funder.rs` | Balance display, solvency checks, top-ups through a root lane. |
| `fees.rs` | The shared fee quote, native and fee-currency quoting, refresh task. |
| `gas.rs` | Analytic gas limits for every transaction shape celox sends. |
| `celo.rs` | Registry, FeeCurrencyDirectory, ERC-20 — via `sol!` contract instances. |
| `keys.rs`, `pacer.rs`, `report.rs` | Key derivation, token bucket, statistics. |
| `workloads/` | Transaction shapes, enum-dispatched. |

Adding a workload: new file in `workloads/`, a variant on both enums, an arm in
`new`/`next`, and a `Config` field for any CLI knob. The module doc states the contract
between a workload and the sender loop — keep it accurate.

## Conventions

- Follow alloy's own idioms; read their docs before writing provider/contract code.
  Contract calls use `sol!` with `#[sol(rpc)]` instances, never hand-rolled
  `abi_encode`/`abi_decode`.
- Gas limits are preset analytically so no transaction pays for `eth_estimateGas`. Exact
  protocol prices where the shape allows; where execution cost is state-dependent, an
  explicit upper bound with a comment saying so.
- Statistics are the product. A change that could make `sent`/`confirmed`/`dropped`/
  `rejected`, the TPS figure or the exit code disagree with the chain is a correctness
  bug, not a cosmetic one.
- Every RPC that a loop depends on needs a timeout; an unbounded call can wedge a task
  where the stop signal cannot reach it.
- Errors that a user will hit should name the thing they got wrong (the env var, the flag,
  the balance), not the internal type that failed.

## Settled — do not relitigate

Sender keys derived from the root private key; env-only `CELOX_PRIVATE_KEY` (no flag);
one shared provider whose wallet holds every sender key; the `Lane` abstraction; the
sender's `select!` loop and its `step` truth table (cancel-safety was verified — do not
restructure the arms); token-bucket pacing; enum-dispatched workloads. CIP-64 is a
workload, not a flag that applies to all workloads.

## Gotchas

- **Localhost hides bugs.** alloy picks its receipt poll interval from the hostname: 250ms
  for `localhost`/`127.0.0.1`/`::1`, **7 seconds** for everything else. celox sets it
  explicitly (`--poll-interval`), but the lesson generalises — a change that looks correct
  against anvil can behave completely differently against a remote endpoint. Ask what a
  fix assumes about latency, fees and block time before calling it verified.
- **anvil cannot serve CIP-64**: no type-0x7b, no fee-currency RPCs. Use the celo-reth dev
  node from `../celo-kona` (`e2e_test/` has the genesis and dev key) or Celo Sepolia.
- **Celo Sepolia (chain 11142220) is the current testnet.** Alfajores and Baklava are dead.
- **Fee currencies are not all 18-decimal.** Read `decimals()` from the token. USDC/USDT
  are registered through adapter contracts that revert on `decimals()` and cannot be
  transferred — celox rejects them.
- **Per-currency intrinsic gas comes from the FeeCurrencyDirectory**, never hardcoded.
- **Nodes cap pending transactions per account** (reth: 16). Raise `--senders`, not
  `--window`, to add throughput.
- **A saturated chain moves the base fee ~12.5% per block**, which is why the fee quote
  refreshes every two seconds rather than every thirty.
- macOS `/usr/bin/env bash` is 3.2: no `&>>` in justfile recipes.
- Logs drop ANSI colour when stdout is not a terminal, so recipes can parse them.
- rand 0.10 renamed the extension trait: `RngExt` for `random_range`, `Rng` for
  `fill_bytes`. `sol!` with a JSON artifact needs alloy-sol-types' `json` feature.

## Dependencies

`celo-alloy-{network,rpc-types}` are git-pinned to celo-kona rev `5bff854` — the tip of
PR #266, which adds CIP-64 client support. No `[patch.crates-io]` block is needed. Repin
to `main` once that PR merges, and revisit whether `cip64_max_fee` can be consumed from
upstream instead of the copy in `fees.rs`.
