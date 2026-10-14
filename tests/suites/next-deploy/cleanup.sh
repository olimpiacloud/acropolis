#!/usr/bin/env bash
# NEXT_TEST_CLEANUP_SCRIPT_PATH: stop the container and drop the image deploy.sh loaded.
set -uo pipefail
[ -f .adapter-deploy.env ] || exit 0
. ./.adapter-deploy.env
docker rm -f "$CONTAINER" >/dev/null 2>&1
docker rmi "$IMAGE" >/dev/null 2>&1
exit 0
