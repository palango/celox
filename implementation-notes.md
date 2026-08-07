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

## Verification strategy (settled with Paul, 2026-08-07)

- **anvil** for all non-CIP-64 work (M1–M3, M5 native mode): standard EIP-1559
  txs, Celo response types decode standard receipts fine. Fast iteration.
- **celo-reth dev node or Celo Sepolia** for the `cip64` workload (M4): anvil
  has no type-0x7b, no fee-currency machinery, no `eth_gasPrice [feeCurrency]`.
  Dev node comes from `celo-kona/e2e_test/run_all_tests.sh` (builds celo-reth,
  --dev, chain 1337, dev key in `shared.sh`; fee currencies 0x…ce16 / 0x…ce17).

## Deviations from plan

(none yet)
