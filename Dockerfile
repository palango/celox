# The Solidity artifact is committed, so no Solidity toolchain is needed here.
# cmake and a C compiler are: the TLS stack (rustls on aws-lc-rs) builds native
# code, and git is needed because celo-alloy is a git dependency.
FROM rust:1.97-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        git \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Compile the dependency tree against a stub binary first. Editing celox's own
# sources then rebuilds only celox, not the several hundred crates beneath it.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# The sol! macro reads this at compile time, relative to the manifest.
COPY contracts/StateWriter.json ./contracts/StateWriter.json
RUN cargo build --release --locked

FROM debian:bookworm-slim

# An HTTPS RPC endpoint needs the trust store; nothing else is required.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 celox

COPY --from=builder /build/target/release/celox /usr/local/bin/celox

USER celox
ENTRYPOINT ["celox"]
CMD ["--help"]
