#!/usr/bin/env bash
set -uo pipefail
A=${ACROPOLIS:-$(cd "$(dirname "$0")/.." && pwd)/target/release/acropolis}
H=/tmp/warm/home
for app in "$@"; do
  rm -rf "/tmp/warm/$app" "$H"
  mkdir -p /tmp/warm
  f=
  cp -r "$(cd "$(dirname "$0")/.." && pwd)/bench/apps/$app" "/tmp/warm/$app"
  for phase in cold same change; do
    if [ $phase = change ]; then
      f=$(cd /tmp/warm/$app && find . -name node_modules -prune -o \( -name '*.go' -o -name '*.rs' -o -name 'page.tsx' -o -name 'App.tsx' -o -name 'index.js' -o -name 'server.js' -o -name 'index.tsx' \) -print | head -1)
      echo "// rebuild $(date +%s%N)" >> "/tmp/warm/$app/$f"
    fi
    s=$(date +%s.%N)
    taskset -c 0-1 $A --home "$H" --events json build "/tmp/warm/$app" -t "localhost:5001/warm/$app:$phase" >/tmp/warm/$app-$phase.log 2>&1 || echo "FAIL $app $phase"
    e=$(date +%s.%N)
    printf "%-15s %-7s %6.1fs  %s\n" "$app" "$phase" "$(echo "$e - $s" | bc)" "${f:-}"
  done
done
