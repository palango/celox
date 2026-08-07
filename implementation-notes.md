# celox implementation notes

Plan: `~/.claude/plans/we-re-writing-a-tool-expressive-starfish.md`.
Fresh start 2026-08-07; the 2026-08-04 celox (checklist + raw-envelope spammer)
is abandoned and its repo is gone.

## Milestone log

### M1 — skeleton (done)

- `Provider<Celo>` stack from celo-kona rev `5bff854` (PR #266 branch tip)
  compiles as a plain git dep, **no `[patch.crates-io]` needed** — confirms the
  PR body's claim. Note the resolver mixes alloy family versions
  (`alloy-json-rpc 2.3.0` next to `alloy-provider =2.0.5`); compiles clean,
  keep an eye on it when bumping.
- Verified against anvil 1.7.1 (`--block-time 1`): self-transfer sent, receipt
  status 1, gas 21000.

### M2 — sender pool (done)

- Modules: `keys` (derivation, golden-address test), `pacer` (token bucket,
  1s burst cap), `funder` (batch top-up with preset nonces), `sender` (one
  task per account: local nonce, `FuturesOrdered` in-flight window, receipt
  timeout), `report` (event aggregation, exit code), `workloads/` (enum
  dispatch, `transfer` = 1-wei self-transfer).
- Verified on anvil (1s blocks): 20 tps × 15s → 320/320 confirmed, exit 0;
  status lines match target rate. Second run topped up only burned gas
  (~0.0015 CELO vs 0.5), proving derived-key balance reuse.
- Simplifications accepted for v1 (revisit if they bite):
  - On a dropped tx the sender drains its window passively (later txs wait
    out their own timeouts) before resyncing the nonce — no active
    cancel/replace. Txs that land after being counted dropped skew stats.
  - Funding happens once at startup; no refill loop during long runs.
  - Fee quote refresh is global, every 30s, native-denominated only (cip64
    fee quoting comes with M4).
  - `Summary::latencies` is unbounded (16 B per confirmed tx): a multi-hour
    high-tps soak run will eat memory. Switch to a histogram if soaks land.
  - Pacer adds one permit per tick with `MissedTickBehavior::Skip`: above a
    few hundred tps, scheduler jitter loses tokens and undershoots the
    target. Refill by elapsed time if high rates become a goal.

### M3 — calldata workload (done)

- `workloads::Calldata`: `--data-size` (default 1024) random bytes to a burner
  EOA `0xcaca…ca`, value 0. Fresh `rand::rng()` per `next` call, so the shared
  `Arc<Workload>` needs no interior mutability.
- Gas is the EIP-7623 floor `21_000 + 10 * tokens`
  (`tokens = zero_bytes + 4 * non_zero_bytes`). A call to an EOA adds no
  execution gas to the standard `21_000 + 4 * tokens`, so the floor always wins
  and is the exact cost — no padding.
- Workload knobs now travel in `workloads::Config`, built from the CLI in
  `main`; `Workload::new(kind, config)`.
- Verified on anvil (1s blocks, `--data-size 2048 --tps 10 --senders 5
  --duration 10s`): 110 sent, 110 confirmed, 0 reverted/dropped/rejected,
  p50 0.72s, exit 0. Receipt spot-check: gas limit 0x19136 (102710) for a
  payload with 7 zero and 2041 non-zero bytes, `gasUsed` identical, status 1 —
  the formula is exact.
- `rand` 0.10, which renamed the core trait `RngCore` to `Rng` and the old `Rng`
  extension trait to `RngExt`; `rand::rng()` and `fill_bytes` are unchanged.

### Codex review of M2 (2026-08-07)

- Fixed: senders bound every submission/nonce RPC with a 10s `RPC_TIMEOUT`
  (a silently hanging RPC could block the loop outside `select!`, deafening
  it to stop); main backstops the drain with `receipt_timeout + 15s` and
  exits 1 instead of hanging CI; `--tps` capped at 10k (beyond that the
  refill interval rounds to zero and panicked inside the detached task).
- Paul decided (2026-08-07): root key comes from `CELOX_PRIVATE_KEY` only;
  the `--private-key` flag is gone (argv leaks into history and ps).
- Accepted stats-semantics quirks, same class as late-landing dropped txs:
  a submission that *times out* (vs. is rejected) counts as Rejected though
  the node may have accepted it; and when the drain deadline fires, the run
  exits 1 without a summary — stuck senders hold event channels open, so the
  reporter cannot finish (a snapshot channel would fix it if it ever bites).
- Deferred: behavioral tests for sender/funder failure paths via alloy's
  mock-transport `Asserter` — proposed as standalone task, not milestone
  filler.

### M4 — cip64 workload (done)

- Verified on celo-reth dev node (chain 1337, 1s blocks): 110/110 confirmed
  at 10 tps, exit 0; sender0 fee-currency balance 0.9970e18 after the run —
  gas debited in ce16, not CELO. Recipe: `just e2e-cip64` (uses bash-3.2-safe
  redirects; macOS `/usr/bin/env bash` may resolve to 3.2).


- `workloads::Cip64`: 1-wei self-transfer with `fee_currency` set; gas =
  `gas::cip64_transfer(intrinsic)` where the per-currency intrinsic surcharge
  comes from the FeeCurrencyDirectory (`getCurrencyConfig`), resolved through
  the registry `0x…ce10` with a mainnet-address fallback for dev chains that
  have no registry. `--fee-currency` is required for (and exclusive to) this
  workload; unlisted currencies and adapter contracts are rejected up front
  (adapters revert on `decimals()` and have no `transfer()` — Paul's
  correction: fee-currency *underlyings* like USDT are 6-decimal; celox reads
  the token's actual `decimals()` and uses it for `--fund-token` parsing and
  display instead of assuming 18).
- `gas` module now owns all intrinsic-gas shapes (`TRANSFER`,
  `calldata_to_eoa`, `cip64_transfer`) — one home, exact prices, no padding.
- Funder: native leg + token leg; shows root balances for both at startup and
  bails before sending anything if either leg is insolvent (Paul's
  requirement: all-or-nothing funding).
- Fee quotes: funder uses a one-shot native quote; senders get a refreshed
  quote denominated in the fee currency via the parameterized
  `eth_gasPrice`/`eth_maxPriorityFeePerGas [feeCurrency]` RPCs with
  CeloGasFiller's cap formula. The filler itself is bypassed by presetting
  (3 RPCs/tx at spam rates); its semantics are kept.
- Alloy style per Paul: contract calls via `sol!` + `#[sol(rpc)]` instances
  (`IErc20`, `IRegistry`, `IFeeCurrencyDirectory`), no manual abi
  encode/decode. Works cleanly with `Provider<Celo>`.
- `just e2e-cip64`: celo-reth dev node (chain 1337, 1s blocks, dev key root);
  asserts exit 0 plus sender0's fee-currency balance strictly between 0 and
  the funded 1e18 — gas debited in the token. Sender0's address is stable
  because derivation is deterministic (`0x47d5…Ed17` for the dev key).

### M5 — storage workload (done; replaced the planned AMM)

- Paul replaced the MiniAmm plan with a simpler `StateWriter` contract:
  `write(start, count)` bumps `count` consecutive mapping slots. Same central
  -state contention (all senders write one contract's storage trie) with far
  less machinery, and no token distribution — so `Workload::setup(senders)`
  was never needed.
- `setup()` is gone entirely: `Workload::new(kind, config, provider)` is async
  and deploys, so a workload is ready to produce transactions the moment it
  exists (no post-setup `Option<Address>`, `next` stays infallible).
- `--writes` (slots per tx, default 5) and `--key-space` (default 1e6). Wide
  key space = fresh slots = state growth; narrow = hot-slot contention.
- `gas.rs` generalized: `contract_call(payload, execution)` implements the
  EIP-7623 max(standard, floor) rule, and `calldata_to_eoa` is now the
  zero-execution case of it. `state_writes` is an **upper bound** (23k/slot:
  fresh slot 22.1k + loop), since a slot's cost depends on whether an earlier
  tx already wrote it. Verified on anvil: limit 141,368 vs used 132,850 — 6%
  over-provisioned, and ~22.3k per fresh write as predicted.
- Contract sources in `contracts/`, artifact committed
  (`contracts/StateWriter.json`), rebuilt via `just build-contracts`; users
  never need a Solidity toolchain. Bound via `sol!` from the artifact path.
- `just e2e-storage` pins `--key-space 1` so every tx bumps slots[0..writes)
  by exactly one; the recipe asserts `slots[0] == confirmed`. Verified:
  220/220.
- Logs now disable ANSI colour when stdout is not a terminal, so recipes can
  parse them.

### Thorough review round (2026-08-07): 4 reviewers, 12 fixes

Four independent reviews at `66e8964` — celo-kona's rust-code-reviewer, an
adversarial-conditions pass, a docs/UX pass, and Codex. Two findings would have
invalidated results the first time celox was pointed at a real chain.

**Fixed:**

1. **Alloy polls receipts every 7s off localhost** (critical). `poll_interval`
   is 250ms for `localhost`/`127.0.0.1`/`::1` and **7000ms** for everything
   else (alloy-rpc-client 2.0.5 client.rs:233). Every milestone was verified on
   127.0.0.1, so this never showed: on Sepolia every latency would quantise to
   multiples of 7s and transactions would be counted dropped at ~14s. Now set
   explicitly via `--poll-interval` (default 250ms), which also lets an operator
   dial down the read load celox puts on the node under test.
2. **A failing receipt watcher was recorded as a drop** (critical). Any
   `get_receipt()` error became `Dropped`, so a transient RPC blip turned a
   landed transaction into a failure, corrupted the counts, and tripped a nonce
   resync. Now falls back to polling by hash until the deadline.
3. **Reported TPS was diluted by the drain**. The divisor spanned spam + drain
   (up to `--receipt-timeout`), understating the rate worst exactly when the
   chain is stressed. Now divides by the sending window.
4. **The pacer could not exceed ~1000 tps.** One permit per tick against
   tokio's 1ms timer granularity capped the offered rate ~10x below the accepted
   `--tps 10000`. Now refills by elapsed time. Measured after: 1438 tps
   sustained on anvil — which is that node's own ceiling (30M ÷ 21k = 1428
   tx/block), so celox is no longer the bottleneck.
5. **`FuturesOrdered` head-of-line blocking.** One slow receipt delayed every
   later result, making status lines bursty and delaying drop detection.
   Nothing depends on completion order, so it is `FuturesUnordered` now.
6. **A chain-controlled value reached a panicking conversion.**
   `intrinsicGas.to::<u64>()` panics (ruint's `to` is `expect`, not truncating);
   reachable via the mainnet-address directory fallback on a non-Celo chain.
   Now `u64::try_from` with celox's own error.
7. **The storage deploy sat outside the all-or-nothing funding guarantee.** The
   solvency check did not include the deploy, so a marginal root could fund
   every sender and then fail to deploy. Deploy now runs before funding.
8. **The fee refresher could wedge forever** on an endpoint that accepts and
   never answers, freezing the quote while the base fee climbed — reintroducing
   the collapse the 2s refresh was meant to fix. Refresh attempts are bounded.
9. **`--writes` was unbounded** and overflowed the analytic gas limit. Capped.
10. **Funding trusted receipt status alone.** An ERC-20 can return `false` or
    skim a fee without reverting. Balances are now re-checked after funding.
11. **`--fund` gives no warning when it cannot cover a window.** Nodes reserve
    `gas_limit × max_fee` per pending transaction, so 0.1 CELO fails on a chain
    with a 200 gwei floor. Now warns before the run.
12. Smaller: status-line rate measured against the real interval rather than the
    nominal 5s; the second-Ctrl-C message no longer claims "again" when it was
    the first; `foundry.toml` pins solc 0.8.25 / cancun so `just
    build-contracts` is reproducible; `MAX_WINDOW`'s comment matches the
    warning-only behaviour.

**Accepted, documented rather than changed:** latency percentiles cover
confirmed transactions only (drops waited the full timeout by definition — the
README now says to read them next to the drop count); a permanently stale fee
quote fails the run through rejections without naming itself as the cause;
`Summary::latencies` is unbounded and cloned per percentile call.

**Still deferred:** replace-by-fee for transactions stranded when the base fee
overtakes their cap; mock-transport tests for `Lane`/`Sender`/`Funder` — Codex
correctly notes the untested surface is broader than the notes claimed, since it
covers the whole submission-to-statistics path, not just failure branches. The
single highest-value test to add: submission succeeds, first receipt query fails
transiently, a later one returns status 1, and the reporter sees exactly one
`Sent` and one `Confirmed` with no `Dropped`.

### M6 — polish (done)

- README covering workloads, the funding/sending/reporting model, throughput
  ceilings and the dev recipes.
- Second Ctrl-C during the drain exits 130 instead of waiting out
  `receipt_timeout`. Verified by stalling a drain (`anvil_setIntervalMining 0`):
  first SIGINT drained and stayed alive, second exited 130 in under a second.
  Graceful path verified separately: 158/158 confirmed, exit 0.
- `--window` above `lane::MAX_WINDOW` now warns rather than failing, since the
  16-per-account cap is a node policy and not universal.

### Base-fee saturation finding (2026-08-07, 100 senders on anvil)

Anvil (30M gas limit, 1s blocks) with 100 senders at 300 tps: blocks filled
completely (~29.9M gas, 226 storage txs/block) and the **base fee rose from
235M to 675M wei over 8 blocks** — so yes, celox can saturate a chain hard
enough to move the base fee.

It also exposed a bug: **the 30s fee-quote refresh cannot track a base fee
that doubles in 8 seconds.** The quote is taken once after funding (base fee
235M → cap ≈ 536M); once the base fee passed that cap, every submission was
rejected with "max fee per gas less than block base fee" (100 rejections, one
per sender) and the 930 already-pending transactions were stranded below the
base fee until they timed out. Run collapsed to 0 tps: 2035/2965 confirmed.

**Fixed**: `fees::REFRESH_INTERVAL` is 2s, not 30s. Same run afterwards: 0
rejections (was 100), 4527 confirmed (was 2035), ~227 tps sustained instead of
collapsing to 0. The remaining 1360 drops are honest — 300 offered tps against
a chain that fits ~226 storage txs per block (30M gas ÷ 133k) — plus the
in-flight backlog timing out during drain. Set `--tps` at or below the chain's
capacity for a drop-free run.

Still deferred: a transaction already *pending* when the base fee overtakes
its cap needs replace-by-fee to recover. A `--fee-multiplier` would buy more
headroom and costs nothing in real fees (EIP-1559 charges base+tip, never the
cap), but it inflates the funder's balance check — which matters on chains
with a high floor like Celo mainnet's 200 gwei. Not added without a need.

### Prior art (checked 2026-08-07)

celo-monorepo has **no** load-test contract — its load test (`packages/celotool`
`geth simulate-client` + `helm-charts/load-test`) only does transfer / data
(large zero-byte calldata) / mixed modes, no contract deploy. Same for
celo-org/celo-loadtest and tx-overload. hardfork-testing's `Bench.sol` is a
CPU burner with `view` functions and hardcoded iteration counts.

Real prior art is ethpandaops/spamoor: `ERC20Bloater.bloatStorage(startSlot,
numAddresses)` is nearly identical to our StateWriter (start + count over fresh
mapping slots), which independently validates the design. Its
`StorageSpam.setRandomForGas(gasLimit, txid)` takes the other approach —
`while (gasleft() > gasLimit)` — which makes gas **exact** rather than an upper
bound and lets a caller dial "N gas per transaction" directly. Worth
considering if our 6% over-provisioning or block-packing fidelity ever matters.

### Lane abstraction (2026-08-07, Paul's idea)

- `lane.rs`: one account's send pipeline — local nonce, bounded in-flight
  window (`FuturesOrdered`), `submit`/`resync`/`next_completed`. The sender
  drives one lane through its paced `select!` loop (structure and `step` truth
  table unchanged, cancel-safety preserved); the funder pipelines the root's
  funding through one lane instead of fixed `chunks(10)` batching. Removes the
  duplicated account-field fill chain and the two copies of the pool-cap
  number — now `lane::MAX_WINDOW = 16` in one place, used as the funder window
  and (soon) the senders' upper bound. `next_completed` stays pending on an
  empty window so it is select-safe without a busy loop. `RPC_TIMEOUT` moved
  here; main's drain deadline references `lane::RPC_TIMEOUT`.
- The funder's failure semantics improved as a side effect: a rejected or
  dropped funding tx now fails fast with a specific message instead of a
  post-hoc receipt scan.

### Post-M4 refactor round (2026-08-07, Paul approved "apply all")

- Applied: `fees.rs` (quote logic + saturating, unit-tested cap formula);
  shared provider/wallet (one HTTP pool, `register_signer` per sender, wallet
  signs by `from`); chain id via cached ChainIdFiller; Config-vs-setup rule
  in the workloads doc; funder batches sends (`FUNDING_BATCH = 10`) after
  10-sender funding hit reth's 16-pending-per-account cap ("txpool is full").
- **Deviation from the approved set:** making `cip64_max_fee` pub on the
  celo-kona PR branch is deferred until PR 266 merges. Consuming it requires
  pushing that branch (which also carries Paul's unpushed test commit
  `2b9e5ce` — his call, not mine) and bumping celox's pin; the drift risk it
  addressed is meanwhile covered by fees.rs matching upstream's saturating
  arithmetic with a unit test. Revisit at the pin bump.
- Architecture review verdicts on record: skip orchestrator module, sender
  context split, Funder struct; M5 needs `setup(root, senders)` since Paul
  chose real-ERC-20-balance AMM swaps.

## Verification strategy (settled with Paul, 2026-08-07)

- **anvil** for all non-CIP-64 work (M1–M3, M5 native mode): standard EIP-1559
  txs, Celo response types decode standard receipts fine. Fast iteration.
- **celo-reth dev node or Celo Sepolia** for the `cip64` workload (M4): anvil
  has no type-0x7b, no fee-currency machinery, no `eth_gasPrice [feeCurrency]`.
  Dev node comes from `celo-kona/e2e_test/run_all_tests.sh` (builds celo-reth,
  --dev, chain 1337, dev key in `shared.sh`; fee currencies 0x…ce16 / 0x…ce17).

## Deviations from plan

(none yet)
