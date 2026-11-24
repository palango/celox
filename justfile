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
    for name in ["StateWriter", "StateProber"]:
        artifact = json.load(open(f"contracts/out/{name}.sol/{name}.json"))
        json.dump(
            {"abi": artifact["abi"], "bytecode": {"object": artifact["bytecode"]["object"]}},
            open(f"contracts/{name}.json", "w"),
            indent=2,
        )
    PY

test:
    cargo test

# Build the container image.
docker-build:
    docker build -t celox .

# End-to-end smoke test against a throwaway anvil node (anvil dev key 0 as root)
e2e:
    #!/usr/bin/env bash
    set -euo pipefail
    lsof -ti :18545 >/dev/null 2>&1 && { echo "ERROR: port 18545 is already in use"; exit 1; }
    anvil --port 18545 --block-time 1 &> /tmp/celox-e2e-anvil.log &
    trap "kill $! 2>/dev/null || true" INT TERM EXIT
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
    lsof -ti :18549 >/dev/null 2>&1 && { echo "ERROR: port 18549 is already in use"; exit 1; }
    anvil --port 18549 --block-time 1 > /tmp/celox-e2e-storage-anvil.log 2>&1 &
    trap "kill $! 2>/dev/null || true" INT TERM EXIT
    sleep 2
    run() {
        NO_COLOR=1 CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
            cargo run -q -- --rpc-url $RPC \
            --workload storage --contracts 3 --writes 5 --key-space 5 --salt 42 \
            --tps 20 --senders 10 --duration 10s 2>&1 | tee /dev/stderr
    }
    contracts_of() { sed -n 's/.*state writer.*contract=\([0-9a-fA-Fx]*\).*/\1/p' | sort; }
    confirmed_of() { sed -n 's/.*run finished.*confirmed=\([0-9]*\).*/\1/p'; }
    FIRST=$(run)
    SECOND=$(run)
    CONTRACTS=$(echo "$FIRST" | contracts_of)
    [[ $(echo "$CONTRACTS" | wc -l) -eq 3 ]] || { echo "ERROR: expected 3 writer contracts, got: $CONTRACTS"; exit 1; }
    # The same --salt must land the second run on the first run's contracts.
    if [[ $(echo "$SECOND" | grep -c "reusing state writer") -ne 3 || "$(echo "$SECOND" | contracts_of)" != "$CONTRACTS" ]]; then
        echo "ERROR: the second run with --salt 42 did not reuse all of: $CONTRACTS"
        exit 1
    fi
    CONFIRMED=$(( $(echo "$FIRST" | confirmed_of) + $(echo "$SECOND" | confirmed_of) ))
    # With writes == key-space every transaction bumps slots[0..5) of one
    # contract, so slots[0] summed over the contracts counts every transaction.
    SLOT=$(cast index uint256 0 0)
    TOTAL=0
    for CONTRACT in $CONTRACTS; do
        TOTAL=$(( TOTAL + $(cast storage --rpc-url $RPC "$CONTRACT" "$SLOT") ))
    done
    if [[ $TOTAL -ne $CONFIRMED ]]; then
        echo "ERROR: slots[0] sums to $TOTAL over the contracts after $CONFIRMED confirmed transactions"
        exit 1
    fi
    echo "OK: second run reused all 3 contracts; sum of slots[0] == $CONFIRMED confirmed transactions"

# Storage prefill e2e: a cut-short prefill fails and leaves slots unwritten, a
# re-run resumes and finishes by itself, a third run finds nothing pending, and
# a chain without the CREATE2 deployer refuses --prefill before funding.
e2e-storage-prefill:
    #!/usr/bin/env bash
    set -euo pipefail
    RPC=http://127.0.0.1:18551
    BARE=http://127.0.0.1:18552
    for PORT in 18551 18552; do
        lsof -ti :$PORT >/dev/null 2>&1 && { echo "ERROR: port $PORT is already in use"; exit 1; }
    done
    anvil --port 18551 --block-time 1 > /tmp/celox-e2e-prefill-anvil.log 2>&1 &
    ANVIL=$!
    anvil --port 18552 --block-time 1 --disable-default-create2-deployer > /tmp/celox-e2e-prefill-bare-anvil.log 2>&1 &
    BARE_ANVIL=$!
    trap "kill $ANVIL $BARE_ANVIL 2>/dev/null || true" INT TERM EXIT
    sleep 2
    prefill() {
        local rpc=$1; shift
        NO_COLOR=1 CELOX_PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
            cargo run -q -- --rpc-url $rpc \
            --workload storage --prefill --salt 7 --contracts 3 --key-space 50 --writes 5 \
            --senders 5 "$@" 2>&1 | tee /dev/stderr
    }
    written() {
        local count=0
        for CONTRACT in $CONTRACTS; do
            for K in $(seq 0 49); do
                [[ $(( $(cast storage --rpc-url $RPC "$CONTRACT" "$(cast index uint256 $K 0)") )) -ne 0 ]] && count=$((count + 1))
            done
        done
        echo $count
    }
    # (a) Cut short: exits non-zero with part of the 150 slots written.
    if OUT=$(prefill $RPC --tps 5 --duration 1s); then echo "ERROR: a cut-short prefill exited 0"; exit 1; fi
    CONTRACTS=$(echo "$OUT" | sed -n 's/.*state writer.*contract=\([0-9a-fA-Fx]*\).*/\1/p')
    echo "$OUT" | grep -q "prefill: 30 of 30 chunks pending" || { echo "ERROR: (a) did not start with 30 pending chunks"; exit 1; }
    A=$(written)
    (( A > 0 && A < 150 )) || { echo "ERROR: (a) wrote $A of 150 slots"; exit 1; }
    # (b) Resume: ends by itself, exits 0, every slot written.
    OUT=$(prefill $RPC --tps 20) || { echo "ERROR: the resumed prefill failed"; exit 1; }
    echo "$OUT" | grep -q "every pending chunk handed out" || { echo "ERROR: (b) did not stop on exhaustion"; exit 1; }
    B=$(written)
    [[ $B -eq 150 ]] || { echo "ERROR: (b) left $((150 - B)) of 150 slots unwritten"; exit 1; }
    # (c) Nothing left: exits 0 without sending.
    OUT=$(prefill $RPC --tps 20) || { echo "ERROR: the finished prefill's re-run failed"; exit 1; }
    echo "$OUT" | grep -q "prefill: 0 of 30 chunks pending" || { echo "ERROR: (c) found chunks pending"; exit 1; }
    echo "$OUT" | grep -q "spamming" && { echo "ERROR: (c) sent transactions"; exit 1; }
    # (d) No CREATE2 deployer: refused before funding.
    if OUT=$(prefill $BARE --tps 20); then echo "ERROR: --prefill ran without the CREATE2 deployer"; exit 1; fi
    echo "$OUT" | grep -q "needs the CREATE2 deployer" || { echo "ERROR: (d) failed for another reason"; exit 1; }
    echo "$OUT" | grep -qi "fund" && { echo "ERROR: (d) reached funding"; exit 1; }
    echo "OK: cut-short prefill wrote $A/150 and failed; resume wrote 150/150 and stopped; re-run found 0 pending; no-deployer chain refused"

# Cold-read e2e: `just e2e-cold-read` probes native balances on anvil;
# `just e2e-cold-read token` probes the fee currency's balances on the celo-reth
# dev node (see e2e-cip64). Every probe must confirm under its preset gas limit,
# and the measured gas per read is printed for the bounds in src/gas.rs.
e2e-cold-read mode="accounts":
    #!/usr/bin/env bash
    set -euo pipefail
    READS=200
    if [[ "{{mode}}" == token ]]; then
        KONA="${KONA:-$(dirname "$(pwd)")/celo-kona}"
        CELO_RETH="$KONA/target/debug/celo-reth"
        [[ -x "$CELO_RETH" ]] || cargo build -p celo-reth --manifest-path "$KONA/Cargo.toml"
        GENESIS="$KONA/e2e_test/celo-dev-genesis.json"
        RPC=http://127.0.0.1:18548
        KEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5
        EXTRA=(--read-token 0x000000000000000000000000000000000000ce16)
        lsof -ti :18548 >/dev/null 2>&1 && { echo "ERROR: port 18548 is already in use"; exit 1; }
        DATADIR=$(mktemp -d)
        LOG=/tmp/celox-e2e-cold-read-celo-reth.log
        "$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" > "$LOG" 2>&1
        "$CELO_RETH" node --dev --dev.block-time 1s --chain "$GENESIS" --datadir "$DATADIR" \
            --http --http.port 18548 --authrpc.port 18648 --disable-discovery >> "$LOG" 2>&1 &
        NODE_PID=$!
        trap 'kill $NODE_PID 2>/dev/null; wait $NODE_PID 2>/dev/null; rm -rf "$DATADIR"; true' INT TERM EXIT
    else
        RPC=http://127.0.0.1:18553
        KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
        EXTRA=()
        lsof -ti :18553 >/dev/null 2>&1 && { echo "ERROR: port 18553 is already in use"; exit 1; }
        anvil --port 18553 --block-time 1 > /tmp/celox-e2e-cold-read-anvil.log 2>&1 &
        NODE_PID=$!
        trap "kill $NODE_PID 2>/dev/null || true" INT TERM EXIT
    fi
    for _ in $(seq 60); do
        cast block-number --rpc-url $RPC &>/dev/null && break
        sleep 0.5
    done
    OUT=$(NO_COLOR=1 CELOX_PRIVATE_KEY=$KEY cargo run -q -- --rpc-url $RPC \
        --workload cold-read --reads $READS "${EXTRA[@]}" \
        --tps 10 --senders 10 --duration 10s 2>&1 | tee /dev/stderr)
    echo "$OUT" | grep -q "run finished.*reverted=0" || { echo "ERROR: a probe reverted"; exit 1; }
    PROBER=$(echo "$OUT" | sed -n 's/.*state prober.*prober=\(0x[0-9a-fA-F]*\).*/\1/p' | tr A-F a-f)
    # One confirmed probe: its preset limit and the gas it actually used.
    HEAD=$(cast block-number --rpc-url $RPC)
    for B in $(seq $HEAD -1 $((HEAD - 20))); do
        TX=$(cast block $B --full --json --rpc-url $RPC \
            | jq -c --arg p "$PROBER" '[.transactions[] | select((.to // "" | ascii_downcase) == $p)][0] // empty')
        [[ -n "$TX" ]] && break
    done
    [[ -n "${TX:-}" ]] || { echo "ERROR: no probe transaction in the last 20 blocks"; exit 1; }
    LIMIT=$(( $(echo "$TX" | jq -r .gas) ))
    USED=$(( $(cast receipt --rpc-url $RPC $(echo "$TX" | jq -r .hash) gasUsed) ))
    (( USED < LIMIT )) || { echo "ERROR: a probe used $USED gas against its $LIMIT limit"; exit 1; }
    echo "OK: {{mode}} probe of $READS reads used $USED gas (~$(( (USED - 21000) / READS )) per read incl. base) under its $LIMIT limit"

# Deploys against a reth txpool, which caps pending transactions per account
# (16): a celo-reth dev node whose genesis also carries the CREATE2 deployer,
# deploying more storage contracts than that cap in one run, then reusing them.
e2e-storage-reth:
    #!/usr/bin/env bash
    set -euo pipefail
    KONA="${KONA:-$(dirname "$(pwd)")/celo-kona}"
    CELO_RETH="$KONA/target/debug/celo-reth"
    [[ -x "$CELO_RETH" ]] || cargo build -p celo-reth --manifest-path "$KONA/Cargo.toml"
    RPC=http://127.0.0.1:18556
    lsof -ti :18556 >/dev/null 2>&1 && { echo "ERROR: port 18556 is already in use"; exit 1; }
    DATADIR=$(mktemp -d)
    GENESIS="$DATADIR/genesis.json"
    jq '.alloc["4e59b44847b379578588920ca78fbf26c0b4956c"] = {"balance": "0x0", "code": "0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffe03601600081602082378035828234f58015156039578182fd5b8082525050506014600cf3"}' \
        "$KONA/e2e_test/celo-dev-genesis.json" > "$GENESIS"
    LOG=/tmp/celox-e2e-storage-reth.log
    "$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR/db" > "$LOG" 2>&1
    "$CELO_RETH" node --dev --dev.block-time 1s --chain "$GENESIS" --datadir "$DATADIR/db" \
        --http --http.port 18556 --authrpc.port 18656 --disable-discovery >> "$LOG" 2>&1 &
    NODE_PID=$!
    trap 'kill $NODE_PID 2>/dev/null; wait $NODE_PID 2>/dev/null; rm -rf "$DATADIR"; true' INT TERM EXIT
    for _ in $(seq 60); do
        cast block-number --rpc-url $RPC &>/dev/null && break
        sleep 0.5
    done
    run() {
        NO_COLOR=1 CELOX_PRIVATE_KEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5 \
            cargo run -q -- --rpc-url $RPC \
            --workload storage --contracts 40 --writes 5 --key-space 5 --salt 3 \
            --tps 10 --senders 5 --duration 5s 2>&1 | tee /dev/stderr
    }
    FIRST=$(run) || { echo "ERROR: deploying 40 contracts against a reth txpool failed"; exit 1; }
    [[ $(echo "$FIRST" | grep -c "state writer deployed.*origin=deployed") -eq 40 ]] \
        || { echo "ERROR: expected 40 CREATE2 deploys"; exit 1; }
    SECOND=$(run) || { echo "ERROR: the reusing run failed"; exit 1; }
    [[ $(echo "$SECOND" | grep -c "reusing state writer") -eq 40 ]] \
        || { echo "ERROR: the second run did not reuse all 40 contracts"; exit 1; }
    echo "OK: 40 CREATE2 deploys through a 16-slot reth txpool, all reused by the next run"

# CIP-64 e2e against a celo-reth dev node (chain 1337, built from ../celo-kona).
# Root = the dev account pre-funded with CELO and the 0x…ce16 fee currency.
# `just e2e-cip64 fresh` runs --recipient fresh and also checks the recipients.
e2e-cip64 recipient="self":
    #!/usr/bin/env bash
    set -euo pipefail
    KONA="${KONA:-$(dirname "$(pwd)")/celo-kona}"
    CELO_RETH="$KONA/target/debug/celo-reth"
    if [[ ! -x "$CELO_RETH" ]]; then
        cargo build -p celo-reth --manifest-path "$KONA/Cargo.toml"
    fi
    GENESIS="$KONA/e2e_test/celo-dev-genesis.json"
    RPC=http://127.0.0.1:18547
    FC=0x000000000000000000000000000000000000ce16
    lsof -ti :18547 >/dev/null 2>&1 && { echo "ERROR: port 18547 is already in use"; exit 1; }
    DATADIR=$(mktemp -d)
    LOG=/tmp/celox-e2e-celo-reth.log
    "$CELO_RETH" init --chain "$GENESIS" --datadir "$DATADIR" > "$LOG" 2>&1
    "$CELO_RETH" node --dev --dev.block-time 1s --chain "$GENESIS" --datadir "$DATADIR" \
        --http --http.port 18547 --authrpc.port 18647 --disable-discovery >> "$LOG" 2>&1 &
    NODE_PID=$!
    trap 'kill $NODE_PID 2>/dev/null; wait $NODE_PID 2>/dev/null; rm -rf "$DATADIR"; true' INT TERM EXIT
    for _ in $(seq 60); do
        cast block-number --rpc-url $RPC &>/dev/null && break
        sleep 0.5
    done
    OUT=$(NO_COLOR=1 CELOX_PRIVATE_KEY=0x2771aff413cac48d9f8c114fabddd9195a2129f3c2c436caa07e27bb7f58ead5 \
        cargo run -q -- --rpc-url $RPC \
        --workload cip64 --fee-currency $FC --recipient {{recipient}} \
        --tps 10 --senders 10 --duration 10s 2>&1 | tee /dev/stderr)
    # Sender 0 derives deterministically from the dev key, so its address is
    # stable. Gas paid in the fee currency must have debited its token balance
    # below the funded target of 1e18.
    SENDER0=0x47d52d00897fBF1AE2de5e650890F2B7eA84Ed17
    BAL=$(cast call --rpc-url $RPC $FC 'balanceOf(address) returns (uint256)' $SENDER0 | awk '{print $1}')
    if ! [[ "$BAL" =~ ^[0-9]+$ ]] || [[ $(echo "$BAL > 0 && $BAL < 1000000000000000000" | bc) -ne 1 ]]; then
        echo "ERROR: sender0 fee-currency balance is '$BAL'; expected 0 < balance < 1e18"
        exit 1
    fi
    echo "OK: sender0 fee-currency balance $BAL (< funded 1e18, gas debited in token)"
    if [[ "{{recipient}}" == fresh ]]; then
        # Every confirmed transaction must have moved exactly 1 wei of the token
        # to an address that held none: as many distinct recipients of 1-wei
        # Transfers as confirmed transactions, each holding exactly 1 wei.
        CONFIRMED=$(echo "$OUT" | sed -n 's/.*run finished.*confirmed=\([0-9]*\).*/\1/p')
        [[ "$CONFIRMED" =~ ^[1-9][0-9]*$ ]] || { echo "ERROR: no confirmed count in celox's summary"; exit 1; }
        ONE=0x0000000000000000000000000000000000000000000000000000000000000001
        RECIPIENTS=$(cast logs --rpc-url $RPC --address $FC --from-block 0 --json \
            'Transfer(address indexed,address indexed,uint256)' \
            | jq -r --arg one $ONE '.[] | select(.data == $one) | .topics[2]' | sort -u)
        COUNT=$(echo "$RECIPIENTS" | rg -c . || echo 0)
        if [[ $COUNT -ne $CONFIRMED ]]; then
            echo "ERROR: $COUNT distinct 1-wei token recipients after $CONFIRMED confirmed transactions"
            exit 1
        fi
        for TOPIC in $RECIPIENTS; do
            ADDR=0x${TOPIC:26}
            HELD=$(cast call --rpc-url $RPC $FC 'balanceOf(address) returns (uint256)' $ADDR | awk '{print $1}')
            if [[ "$HELD" != 1 ]]; then
                echo "ERROR: recipient $ADDR holds '$HELD' of the token; expected exactly 1"
                exit 1
            fi
        done
        echo "OK: $CONFIRMED confirmed transactions, $COUNT fresh recipients holding 1 wei each"
    fi
