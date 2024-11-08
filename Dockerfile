FROM rustlang/rust:nightly AS chef

RUN cargo install cargo-chef
WORKDIR /app

COPY Cargo.toml Cargo.lock ./

RUN cargo chef prepare --recipe-path recipe.json
RUN cargo chef cook --recipe-path recipe.json

COPY . .

RUN cargo build --release

FROM debian:bookworm-slim AS runtime
WORKDIR /app
COPY --from=chef /app/target/release/hyperlane-relayer-blueprint /usr/local/bin

LABEL org.opencontainers.image.authors="Webb Technologies Inc."
LABEL org.opencontainers.image.description="A Tangle Blueprint (AVS) for deploying Hyperlane relayers"
LABEL org.opencontainers.image.source="https://github.com/tangle-network/hyperlane-relayer-blueprint"

ENV RUST_LOG="gadget=info"
ENV BIND_ADDR="0.0.0.0"
ENV BIND_PORT=9632
ENV BLUEPRINT_ID=0
ENV SERVICE_ID=0

ENTRYPOINT ["/usr/local/bin/hyperlane-relayer-blueprint"]