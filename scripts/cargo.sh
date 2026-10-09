#!/usr/bin/env bash
# Runs cargo in its own systemd unit (memory cap, OOMPolicy=continue) so an OOM kills the build, not the session.
# Falls back to plain cargo without systemd as init, without root, or with ACROPOLIS_NO_SYSTEMD=1.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-1}"
if [ "${ACROPOLIS_NO_SYSTEMD:-0}" = 1 ] || [ ! -d /run/systemd/system ] || [ "$(id -u)" != 0 ] || ! command -v systemd-run >/dev/null; then
  exec nice -n 10 cargo "$@"
fi
exec systemd-run --quiet --wait --pipe --collect --unit "acropolis-cargo-$$" \
  -p OOMPolicy=continue -p MemoryMax="${MEM:-5G}" -p MemorySwapMax=1G ${CPUS:+-p AllowedCPUs=$CPUS} \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" --setenv=CARGO_TERM_COLOR=never --setenv=CARGO_INCREMENTAL="$CARGO_INCREMENTAL" \
  nice -n 10 cargo "$@"
