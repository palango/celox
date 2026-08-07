# Format and lint by default
default: lint

# Format code and run clippy on everything, failing on warnings
lint:
    cargo fmt
    cargo clippy --all-targets -- -D warnings

build:
    cargo build

# Recompile the Solidity contracts and refresh the committed artifacts, so
# users never need a Solidity toolchain to build celox.
build-contracts:
    #!/usr/bin/env bash
    set -euo pipefail
    (cd contracts && forge build)
    python3 - <<'PY'
    import json
    artifact = json.load(open("contracts/out/StateWriter.sol/StateWriter.json"))
    json.dump(
        {"abi": artifact["abi"], "bytecode": {"object": artifact["bytecode"]["object"]}},
        open("contracts/StateWriter.json", "w"),
        indent=2,
    )
    PY

test:
    cargo test

# End-to-end smoke test against a throwaway anvil node (anvil dev key 0 as root)
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    anvil --port 18545 --block-time 1 &> /tmp/celox-e2e-anvil.log &
    trap "kill $! 2>/dev/null" EXIT
    sleep 2
    CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
        cargo run -q -- --rpc-url http://127.0.0.1:18545 \
        --tps 20 --senders 10 --duration 10s

# Storage-workload e2e against a throwaway anvil node. Pins --key-space 1 so
# every transaction bumps slots[0..writes) by exactly one, making the contract's
# final slot value equal the number of confirmed transactions.
e2e-storage:
    #!/usr/bin/env bash
    set -euo pipefail
    RPC=http://127.0.0.1:18549
    anvil --port 18549 --block-time 1 > /tmp/celox-e2e-storage-anvil.log 2>&1 &
    trap "kill $! 2>/dev/null" EXIT
    sleep 2
    OUT=$(NO_COLOR=1 CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
        cargo run -q -- --rpc-url $RPC \
        --workload storage --writes 5 --key-space 1 \
        --tps 20 --senders 10 --duration 10s 2>&1 | tee /dev/stderr)
    CONTRACT=$(echo "$OUT" | sed -n 's/.*state writer deployed.*contract=\([0-9a-fA-Fx]*\).*/\1/p')
    CONFIRMED=$(echo "$OUT" | sed -n 's/.*run finished.*confirmed=\([0-9]*\).*/\1/p')
    # slots[0] is bumped once per transaction, so it must equal the confirmed count.
    SLOT=$(cast index uint256 0 0)
    VALUE=$(cast storage --rpc-url $RPC "$CONTRACT" "$SLOT")
    if [[ $((VALUE)) -ne $CONFIRMED ]]; then
        echo "ERROR: slots[0] is $((VALUE)) after $CONFIRMED confirmed transactions"
        exit 1
    fi
    echo "OK: slots[0] == $CONFIRMED confirmed transactions (state writes landed)"

# CIP-64 e2e against a celo-reth dev node (chain 1337, built from ../celo-kona).
# Root = the dev account pre-funded with CELO and the 0x…ce16 fee currency.
e2e-cip64:
    #!/usr/bin/env bash
    set -euo pipefail
    KONA=/Users/paul/Projects/celo-kona
    CELO_RETH="$KONA/target/debug/celo-reth"
    if [[ ! -x "$CELO_RETH" ]]; then
        cargo build -p celo-reth --manifest-path "$KONA/Cargo.toml"
    fi
    GENESIS="$KONA/e2e_test/celo-dev-genesis.json"
    RPC=http://127.0.0.1:18547
    FC=0x000000000000000000000000000000000000ce16
    DATADIR=$(mktemp -d)
    LOG=/tmp/celox-e2e-celo-reth.log
    "$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" > "$LOG" 2>&1
    "$CELO_RETH" node --dev --dev.block-time 1s --chain "$GENESIS" --datadir "$DATADIR" \
        --http --http.port 18547 --authrpc.port 18647 --disable-discovery >> "$LOG" 2>&1 &
    NODE_PID=$!
    trap 'kill $NODE_PID 2>/dev/null; wait $NODE_PID 2>/dev/null; rm -rf "$DATADIR"' EXIT
    for _ in $(seq 60); do
        cast block-number --rpc-url $RPC &>/dev/null && break
        sleep 0.5
    done
    CELOX_PRIVATE_KEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5 \
        cargo run -q -- --rpc-url $RPC \
        --workload cip64 --fee-currency $FC \
        --tps 10 --senders 10 --duration 10s
    # Sender 0 derives deterministically from the dev key, so its address is
    # stable. Gas paid in the fee currency must have debited its token balance
    # below the funded target of 1e18.
    SENDER0=0x47d52d00897fBF1AE2de5e650890F2B7eA84Ed17
    BAL=$(cast call --rpc-url $RPC $FC 'balanceOf(address) returns (uint256)' $SENDER0 | awk '{print $1}')
    if [[ "$BAL" == "1000000000000000000" || "$BAL" == "0" ]]; then
        echo "ERROR: sender0 fee-currency balance $BAL - gas was not paid in the fee currency"
        exit 1
    fi
    echo "OK: sender0 fee-currency balance $BAL (< funded 1e18, gas debited in token)"
