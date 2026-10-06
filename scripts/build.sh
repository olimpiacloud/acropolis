#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
exec systemd-run --quiet --wait --pipe --collect --unit "acro-cargo-$$" \
  -p OOMPolicy=continue -p MemoryMax="${MEM:-5G}" -p MemorySwapMax=1G \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=CARGO_TERM_COLOR=never \
  nice -n 10 cargo build --profile "${PROFILE:-fast}" "$@"
