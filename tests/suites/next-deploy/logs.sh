#!/usr/bin/env bash
# NEXT_TEST_DEPLOY_LOGS_SCRIPT_PATH: the ID markers the harness parses first (BUILD_ID, DEPLOYMENT_ID,
# NEXT_SUPPORTS_IMMUTABLE_ASSETS for Next >= 16.3, IMMUTABLE_ASSET_TOKEN for 16.2), then the build and server logs.
set -uo pipefail
[ -f .adapter-ids.log ] && cat .adapter-ids.log
[ -f .adapter-build.log ] && { echo "=== build"; cat .adapter-build.log; }
if [ -f .adapter-deploy.env ]; then
  . ./.adapter-deploy.env
  echo "=== server ($CONTAINER)"
  docker logs "$CONTAINER" 2>&1
fi
exit 0
