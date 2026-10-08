#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
EXT="${ACROPOLIS_EXT:-$PWD/../acropolis-ext}"
UNIT=acropolis-bench

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
        if r["tool"] in ("acropolis", "acro"):
            r["tool"] = f"acropolis@{name}"
        out.append(json.dumps(r))
print("\n".join(out))
PY
}

case "${1:-}" in
  report)
    shift
    [ $# -gt 0 ] || set -- bench/results/baseline-2cpu-91378f6.jsonl $(ls -t bench/results/acropolis-*.jsonl 2>/dev/null | head -1)
    tmp=$(mktemp --suffix=.jsonl)
    label "$@" > "$tmp"
    target/"${PROFILE:-release}"/acropolis report "$tmp" 2>/dev/null || target/fast/acropolis report "$tmp"
    rm -f "$tmp"
    exit 0 ;;
  status)
    systemctl is-active "$UNIT" || true
    tail -3 "$(ls -t /tmp/acropolis-bench-*.log | head -1)" 2>/dev/null || true
    exit 0 ;;
esac

if systemctl is-active --quiet "$UNIT"; then
  echo "$UNIT is already running" >&2
  exit 1
fi
PROFILE="${PROFILE:-release}" scripts/build.sh
rev=$(git rev-parse --short HEAD)
git diff --quiet HEAD -- crates Cargo.toml Cargo.lock || rev="$rev-dirty"
stamp=$(date +%Y%m%d-%H%M%S)
prev=$(ls -t "$EXT"/bin/acropolis-bench-* 2>/dev/null | head -1 || true)
bin="$EXT/bin/acropolis-bench-$rev-$stamp"
cp target/"${PROFILE:-release}"/acropolis "$bin"
ls -t "$EXT"/bin/acropolis-bench-* 2>/dev/null | tail -n +5 | xargs -r rm -f || true
tools="${TOOLS:-acropolis}"
out="bench/results/acropolis-$rev-$stamp.jsonl"
if [ "${AB:-0}" = 1 ]; then
  base="${AB_BASE:-$prev}"
  [ -n "$base" ] || { echo "AB=1 needs a previous acropolis-bench binary in $EXT/bin (or AB_BASE=path)" >&2; exit 1; }
  plabel=$(basename "$base" | sed -E 's/^acropolis-bench-//')
  tools="acropolis@$plabel=$base,acropolis@$rev-$stamp=$bin"
  out="bench/results/ab-$plabel-vs-$rev-$stamp.jsonl"
elif [ "$tools" != acropolis ]; then
  out="bench/results/full-${SCENARIO:-both}-$rev-$stamp.jsonl"
fi
log="/tmp/acropolis-bench-$stamp.log"

systemd-run --quiet --collect --unit "$UNIT" -p OOMPolicy=continue \
  --working-directory="$PWD" --setenv=PATH="$PATH" --setenv=HOME="$HOME" \
  -p StandardOutput="file:$log" -p StandardError="file:$log" \
  "$bin" bench --apps "${APPS:-express-api,go-api,rust-api,vite-react,vite-mui,tanstack-start,next15}" \
  --tools "$tools" --runs "${RUNS:-2}" --cpus "${CPUS:-0-1}" --mirror "${MIRROR:-off}" --scenario "${SCENARIO:-both}" ${FRESH:+--fresh} \
  ${APPS_FILE:+--apps-file "$APPS_FILE"} --repo . --railpack "$EXT/tools/railpack" --out "$out" "$@"

echo "started $UNIT: $out (log $log)"
if [ "${WAIT:-0}" = 1 ]; then
  while systemctl is-active --quiet "$UNIT"; do sleep 15; done
  if [ "${AB:-0}" = 1 ]; then scripts/bench.sh report "$out"; else scripts/bench.sh report bench/results/baseline-2cpu-91378f6.jsonl "$out"; fi
fi
