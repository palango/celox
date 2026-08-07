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
