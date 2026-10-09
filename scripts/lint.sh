#!/usr/bin/env bash
# CI parity: formatting and clippy with warnings as errors. Extra args go to cargo clippy (e.g. --exclude acropolis-bundle).
set -uo pipefail
cd "$(dirname "$0")/.."
status=0
cargo fmt --all --check || status=1
scripts/cargo.sh clippy --workspace --all-targets --locked "$@" -- -D warnings || status=1
exit $status
