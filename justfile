# Format and lint by default
default: lint

# Format code and run clippy on everything, failing on warnings
lint:
    cargo fmt
    cargo clippy --all-targets -- -D warnings

build:
    cargo build

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
        --tps 20 --senders 5 --duration 10s
