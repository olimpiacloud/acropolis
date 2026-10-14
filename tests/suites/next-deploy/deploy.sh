#!/usr/bin/env bash
# NEXT_TEST_DEPLOY_SCRIPT_PATH for the Next.js deploy tests (NEXT_TEST_MODE=deploy).
# The harness runs it with cwd set to the isolated test app: build it with Acropolis (OCI tar, like Olimpia),
# `docker load` + `docker run` it on a free local port and print the URL. Only the URL goes to stdout; the
# rest goes to stderr and to files in the app dir that logs.sh replays.
set -euo pipefail
: "${ACROPOLIS_BIN:?set ACROPOLIS_BIN (run.sh does)}"
home="${ACROPOLIS_DEPLOY_HOME:-/tmp/acropolis-next-deploy-home}"
id="nd-$(basename "$PWD" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9\n' '-' | cut -c1-40)-$$"
image="acropolis-next-deploy/$id:latest"
deployment_id="dpl-$id"

# The app's variables are the ones the harness put on top of run.sh's own environment (`createNext({ env })`);
# the harness's and jest's own variables stay out of the build and the container.
mapfile -d '' app_env < <(python3 - "${NEXT_DEPLOY_BASELINE_ENV:-/dev/null}" <<'PY'
import os, sys
base = {}
data = open(sys.argv[1], "rb").read().decode(errors="replace")
for kv in data.split("\0"):
    if "=" in kv:
        k, v = kv.split("=", 1)
        base[k] = v
skip_prefix = ("JEST_", "NEXT_TEST_", "npm_", "__NEXT_TEST", "PLAYWRIGHT", "NEXT_DEPLOY_", "ACROPOLIS_")
skip = {"NODE_ENV", "PWD", "OLDPWD", "SHLVL", "_", "INIT_CWD", "NODE_OPTIONS", "FORCE_COLOR", "COLOR", "PATH", "HOME"}
for k, v in sorted(os.environ.items()):
    if k in skip or k.startswith(skip_prefix) or base.get(k) == v:
        continue
    sys.stdout.write(f"{k}={v}\0")
PY
)
args=(-e NEXT_TELEMETRY_DISABLED=1 -e NEXT_PRIVATE_TEST_MODE=e2e -e "NEXT_DEPLOYMENT_ID=$deployment_id")
for kv in "${app_env[@]}"; do args+=(-e "$kv"); done

# The harness writes `build` (next build + its post-build marker script) but no `start`, which Vercel doesn't need
# and Acropolis (like Railpack) does: give the app the one `create-next-app` writes.
node -e '
const fs = require("fs");
const pkg = JSON.parse(fs.readFileSync("package.json", "utf8"));
pkg.scripts = pkg.scripts || {};
if (!pkg.scripts.start) {
  pkg.scripts.start = "next start";
  fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
}'

{
  echo "=== acropolis build ($(date -u +%FT%TZ))"
  echo "app env: ${app_env[*]%%=*}"
} > .adapter-build.log
# Diagnostics for every deployment, failed ones too, next to the run's results. A failed deploy gets no URL, so the
# harness never calls cleanup.sh for it: drop its container and image here.
on_exit() {
  local code=$?
  if [ -n "${NEXT_DEPLOY_BUILDS:-}" ]; then
    local keep
    keep="$(dirname "$NEXT_DEPLOY_BUILDS")/apps/$id"
    mkdir -p "$keep"
    cp package.json .acropolis-info.json .adapter-build.log "$keep/" 2>/dev/null || true
    docker history --no-trunc --format '{{.Size}}\t{{.CreatedBy}}' "$image" > "$keep/layers.tsv" 2>/dev/null || true
  fi
  if [ "$code" != 0 ]; then
    docker rm -f "$id" >/dev/null 2>&1 || true
    docker rmi "$image" >/dev/null 2>&1 || true
    rm -f .acropolis-image.tar
  fi
}
trap on_exit EXIT
start=$(date +%s.%N)
# Every test app is built once: a per-app cache would only fill the disk (~0.5 GB of node_modules each). The store
# (tarballs, toolchains) is still shared.
export ACROPOLIS_NO_CACHE=1
if ! "$ACROPOLIS_BIN" --home "$home" build . --oci .acropolis-image.tar --info .acropolis-info.json "${args[@]}" \
  >> .adapter-build.log 2>&1; then
  tail -40 .adapter-build.log >&2
  exit 1
fi
build_s=$(python3 -c "import time; print(round(time.time() - $start, 1))")
# Compressed size, as pushed (the e2e reports the registry size the same way).
image_mb=$(stat -c %s .acropolis-image.tar | python3 -c 'import sys; print(round(int(sys.stdin.read()) / 1e6, 1))')
loaded=$(docker load -i .acropolis-image.tar)
echo "docker load: $loaded" >> .adapter-build.log
rm -f .acropolis-image.tar
ref=$(printf '%s\n' "$loaded" | sed -n 's/^Loaded image: //p; s/^Loaded image ID: //p' | head -1)
[ -n "$ref" ] || { echo "docker load printed no image: $loaded" >&2; exit 1; }
docker tag "$ref" "$image"
# Drop the name the OCI tar carried (an ID would take the image with it).
case "$ref" in sha256:* | "$image") ;; *) docker rmi "$ref" >/dev/null 2>&1 || true ;; esac

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
run_env=(-e PORT=3000 -e HOSTNAME=0.0.0.0)
for kv in "${args[@]}"; do [ "$kv" = -e ] || run_env+=(-e "$kv"); done
docker run -d --name "$id" -p "127.0.0.1:$port:3000" "${run_env[@]}" "$image" >/dev/null
printf 'CONTAINER=%s\nIMAGE=%s\n' "$id" "$image" > .adapter-deploy.env

url="http://127.0.0.1:$port"
for _ in $(seq 1 300); do
  code=$(curl -s -o /dev/null -m 2 -w '%{http_code}' "$url/" || true)
  [ "$code" != 000 ] && break
  if [ "$(docker inspect -f '{{.State.Running}}' "$id" 2>/dev/null)" != true ]; then
    docker logs "$id" 2>&1 | tail -40 >&2
    echo "container $id exited before serving" >&2
    exit 1
  fi
  sleep 0.2
done

workdir=$(docker inspect -f '{{.Config.WorkingDir}}' "$image")
build_id=$(docker cp "$id:${workdir:-/app}/.next/BUILD_ID" - 2>/dev/null | tar -xO 2>/dev/null || true)
{
  echo "BUILD_ID: ${build_id:-unknown}"
  echo "DEPLOYMENT_ID: $deployment_id"
  echo "NEXT_SUPPORTS_IMMUTABLE_ASSETS: 0"
  echo "IMMUTABLE_ASSET_TOKEN: undefined"
} > .adapter-ids.log
printf '%s\t%s\t%s\t%s\n' "$PWD" "$build_s" "$image_mb" "$(date -u +%FT%TZ)" >> "${NEXT_DEPLOY_BUILDS:-/dev/null}"
echo "$url"
