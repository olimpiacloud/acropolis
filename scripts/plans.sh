#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/.."
EXAMPLES="${EXAMPLES:-${ACROPOLIS_EXT:-$PWD/../acropolis-ext}/railpack/examples}"
BIN="${BIN:-target/fast/acropolis}"
DIR=tests/plans
mode="${1:-check}"
mkdir -p "$DIR"
changed=0
total=0
for ex in $(ls "$EXAMPLES"); do
  [ -d "$EXAMPLES/$ex" ] || continue
  total=$((total + 1))
  out=$("$BIN" --home /tmp/acropolis-plans-home plan "$EXAMPLES/$ex" --json 2>&1 | python3 -c '
import json, sys
text = sys.stdin.read()
try:
    p = json.loads(text)
except ValueError:
    lines = [l for l in text.strip().splitlines() if l.startswith("error:")] or text.strip().splitlines() or [""]
    print(json.dumps({"error": lines[0]}, indent=1))
    sys.exit()
p.pop("hash", None)
for s in p.get("steps", []):
    s.pop("hash", None)
print(json.dumps(p, indent=1, sort_keys=True))
')
  if [ "$mode" = update ]; then
    printf '%s\n' "$out" > "$DIR/$ex.json"
  elif ! diff -q <(printf '%s\n' "$out") "$DIR/$ex.json" >/dev/null 2>&1; then
    changed=$((changed + 1))
    echo "changed: $ex"
    [ "${VERBOSE:-0}" = 1 ] && diff <(printf '%s\n' "$out") "$DIR/$ex.json" | head -20
  fi
done
if [ "$mode" = update ]; then
  echo "updated $total plan snapshots in $DIR"
else
  echo "$changed of $total plans changed"
  [ "$changed" = 0 ]
fi
