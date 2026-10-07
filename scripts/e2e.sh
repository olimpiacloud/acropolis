#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
EXT="${ACRO_EXT:-/root/personal/acro-ext}"
EXAMPLES="${EXAMPLES:-$EXT/railpack/examples}"
HOME_DIR="${E2E_HOME:-/tmp/acro-e2e-home}"
UNIT=acro-e2e

case "${1:-}" in
  status)
    state=$(systemctl is-active "$UNIT" 2>/dev/null || true)
    log=$(ls -t /tmp/acro-e2e-*.log 2>/dev/null | head -1 || true)
    echo "unit: $state  log: ${log:-none}"
    [ -n "$log" ] || exit 0
    echo "cases: $(grep -c '^\[e2e\] \(pass\|fail\|skip\)' "$log" || true)  pass: $(grep -c '^\[e2e\] pass' "$log" || true)  fail: $(grep -c '^\[e2e\] fail' "$log" || true)"
    grep -E '^\[e2e\] (fail|retry|low|registry)|^e2e:' "$log" | cut -c1-200 || true
    exit 0 ;;
  stop)
    systemctl stop "$UNIT"; exit 0 ;;
esac

if systemctl is-active --quiet "$UNIT"; then
  echo "$UNIT is already running (scripts/e2e.sh status | stop)" >&2
  exit 1
fi
[ -x target/"${PROFILE:-fast}"/acro ] || { echo "build first: scripts/build.sh" >&2; exit 1; }

rev=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- crates Cargo.toml Cargo.lock || rev="$rev-dirty"
stamp=$(date +%Y%m%d-%H%M%S)
mkdir -p "$EXT/bin"
bin="$EXT/bin/acro-e2e-$rev-$stamp"
cp target/"${PROFILE:-fast}"/acro "$bin"
ls -t "$EXT"/bin/acro-e2e-* 2>/dev/null | tail -n +4 | xargs -r rm -f || true
out="tests/results/railpack-e2e-$rev-$stamp.jsonl"
log="/tmp/acro-e2e-$stamp.log"
ls -t /tmp/acro-e2e-*.log 2>/dev/null | tail -n +6 | xargs -r rm -f || true

systemd-run --quiet --collect --unit "$UNIT" \
  -p OOMPolicy=continue -p MemoryMax="${MEM:-6G}" -p MemorySwapMax=1G \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  -p StandardOutput="file:$log" -p StandardError="file:$log" \
  nice -n 5 "$bin" --home "$HOME_DIR" e2e --examples "$EXAMPLES" --jobs "${JOBS:-4}" --out "$out" "$@"

echo "started $UNIT: binary $bin"
echo "  results: $out"
echo "  log:     $log"
echo "  progress: scripts/e2e.sh status"
if [ "${WAIT:-0}" = 1 ]; then
  while systemctl is-active --quiet "$UNIT"; do sleep 20; done
  grep '^e2e:' "$log" || tail -5 "$log"
fi
