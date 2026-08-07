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
