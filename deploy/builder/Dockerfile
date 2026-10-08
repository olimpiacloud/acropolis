FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked --bin acropolis && strip target/release/acropolis

FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates git python3 make g++ pkg-config xz-utils zstd \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/acropolis /usr/local/bin/acropolis
ENV ACROPOLIS_HOME=/var/lib/acropolis
ARG PREWARM="node:lts,go:latest,bun:latest,uv:latest,python:3.13"
RUN acropolis prewarm --tools "$PREWARM"
ENTRYPOINT ["acropolis"]
