#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
EXT="${ACRO_EXT:-/root/personal/acro-ext}"
UNIT=acro-bench

label() {
  python3 - "$@" <<'PY'
import json, sys, os
out = []
for path in sys.argv[1:]:
    name = os.path.basename(path).removesuffix(".jsonl")
    for line in open(path):
        if not line.strip():
            continue
        r = json.loads(line)
        if r["tool"] == "acro":
            r["tool"] = f"acro@{name}"
        out.append(json.dumps(r))
print("\n".join(out))
PY
}

case "${1:-}" in
  report)
    shift
    [ $# -gt 0 ] || set -- bench/results/baseline-2cpu-91378f6.jsonl $(ls -t bench/results/acro-*.jsonl 2>/dev/null | head -1)
    tmp=$(mktemp --suffix=.jsonl)
    label "$@" > "$tmp"
    target/"${PROFILE:-release}"/acro report "$tmp" 2>/dev/null || target/fast/acro report "$tmp"
    rm -f "$tmp"
    exit 0 ;;
  status)
    systemctl is-active "$UNIT" || true
    tail -3 "$(ls -t /tmp/acro-bench-*.log | head -1)" 2>/dev/null || true
    exit 0 ;;
esac

if systemctl is-active --quiet "$UNIT"; then
  echo "$UNIT is already running" >&2
  exit 1
fi
PROFILE=release scripts/build.sh
rev=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- crates Cargo.toml Cargo.lock || rev="$rev-dirty"
stamp=$(date +%Y%m%d-%H%M%S)
prev=$(ls -t "$EXT"/bin/acro-bench-* 2>/dev/null | head -1 || true)
bin="$EXT/bin/acro-bench-$rev-$stamp"
cp target/release/acro "$bin"
ls -t "$EXT"/bin/acro-bench-* 2>/dev/null | tail -n +5 | xargs -r rm -f || true
tools="${TOOLS:-acro}"
out="bench/results/acro-$rev-$stamp.jsonl"
if [ "${AB:-0}" = 1 ]; then
  base="${AB_BASE:-$prev}"
  [ -n "$base" ] || { echo "AB=1 needs a previous acro-bench binary in $EXT/bin (or AB_BASE=path)" >&2; exit 1; }
  plabel=$(basename "$base" | sed -E 's/^acro-bench-//')
  tools="acro@$plabel=$base,acro@$rev-$stamp=$bin"
  out="bench/results/ab-$plabel-vs-$rev-$stamp.jsonl"
elif [ "$tools" != acro ]; then
  out="bench/results/full-${SCENARIO:-cold}-$rev-$stamp.jsonl"
fi
log="/tmp/acro-bench-$stamp.log"

systemd-run --quiet --collect --unit "$UNIT" -p OOMPolicy=continue \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  -p StandardOutput="file:$log" -p StandardError="file:$log" \
  "$bin" bench --apps "${APPS:-express-api,go-api,rust-api,vite-react,vite-mui,tanstack-start,next15}" \
  --tools "$tools" --runs "${RUNS:-2}" --cpus "${CPUS:-0-1}" --mirror "${MIRROR:-off}" --scenario "${SCENARIO:-cold}" \
  --repo . --railpack "$EXT/tools/railpack" --out "$out" "$@"

echo "started $UNIT: $out (log $log)"
if [ "${WAIT:-0}" = 1 ]; then
  while systemctl is-active --quiet "$UNIT"; do sleep 15; done
  if [ "${AB:-0}" = 1 ]; then scripts/bench.sh report "$out"; else scripts/bench.sh report bench/results/baseline-2cpu-91378f6.jsonl "$out"; fi
fi
