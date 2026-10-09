# syntax=docker/dockerfile:1
# Base images are pinned by digest. To bump one, resolve the tag and replace the digest:
#   docker buildx imagetools inspect rust:1.96-bookworm      (keep in step with rust-version in Cargo.toml)
#   docker buildx imagetools inspect debian:bookworm-slim
FROM rust:1.96-bookworm@sha256:a339861ae23e9abb272cea45dfafde21760d2ce6577a70f8a926153677902663 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin acropolis \
 && cp target/release/acropolis /usr/local/bin/acropolis

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
LABEL org.opencontainers.image.title="acropolis-builder" \
      org.opencontainers.image.description="Acropolis: daemonless app-to-OCI-image builder" \
      org.opencontainers.image.source="https://github.com/olimpiacloud/acropolis" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates git python3 make g++ pkg-config xz-utils zstd tini \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/acropolis /usr/local/bin/acropolis
ENV ACROPOLIS_HOME=/var/lib/acropolis
ARG PREWARM="node:lts,go:latest,bun:latest,uv:latest,python:3.13"
RUN acropolis prewarm --tools "$PREWARM"
# Runs as root on purpose: builds need mount and PID namespaces and overlayfs (CAP_SYS_ADMIN), and write
# layers with the app's ownership. Isolation comes from one throwaway container per build.
# tini as PID 1 forwards SIGTERM and reaps orphans of unisolated build steps.
ENTRYPOINT ["/usr/bin/tini", "--", "acropolis"]
