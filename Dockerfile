# ---- build ----
FROM rust:1-slim AS build
WORKDIR /app

# Native deps for solana-client (TLS, protobuf) and the build.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev protobuf-compiler ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy only what the server binary needs to compile. `include_str!` in
# src/bin/server/main.rs pulls in ../../../static/index.html at compile time, so static/
# must be present during the build.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
COPY static ./static

RUN cargo build --release --bin server --features server

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /app/target/release/server /usr/local/bin/svmscope-server

# The server reads HOST/PORT/SVMSCOPE_RPC_URL from the environment.
ENV HOST=0.0.0.0 PORT=3000
EXPOSE 3000

CMD ["svmscope-server"]
