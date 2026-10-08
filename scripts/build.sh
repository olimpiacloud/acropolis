#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
exec systemd-run --quiet --wait --pipe --collect --unit "acropolis-cargo-$$" \
  -p OOMPolicy=continue -p MemoryMax="${MEM:-5G}" -p MemorySwapMax=1G ${CPUS:+-p AllowedCPUs=$CPUS} \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=CARGO_TERM_COLOR=never --setenv=CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-1}" \
  nice -n 10 cargo build --profile "${PROFILE:-fast}" "$@"
