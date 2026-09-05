# ---- build ----
# Pinned to bookworm: the runtime stage below is bookworm (glibc 2.36), and the
# unpinned rust:1-slim moved to trixie (glibc 2.41), producing binaries that
# fail at startup with "GLIBC_2.38 not found".
FROM rust:1-slim-bookworm AS build
WORKDIR /app

# Native deps for solana-client (TLS, protobuf) and the build. litesvm 0.16
# (via agave-precompiles → openssl/vendored) builds OpenSSL from source, which
# needs full perl (FindBin) and make — perl-base in the slim image isn't enough.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev protobuf-compiler ca-certificates perl make \
    && rm -rf /var/lib/apt/lists/*

# Copy only what the server crate needs to compile. `include_str!` in
# server/src/main.rs pulls in ../../static/index.html at compile time, so static/
# must be present during the build.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY server ./server
COPY static ./static

RUN cargo build --release -p svmscope-server

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/server /usr/local/bin/svmscope-server

# Drop root: the server needs no privileges at runtime.
RUN useradd --system --user-group --no-create-home svmscope
USER svmscope

# The server reads HOST/PORT/SVMSCOPE_RPC_URL from the environment.
ENV HOST=0.0.0.0 PORT=3000
EXPOSE 3000

CMD ["svmscope-server"]
