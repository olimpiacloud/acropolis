#!/usr/bin/env bash
exec "$(dirname "$0")/cargo.sh" test --profile "${PROFILE:-fast}" "$@"
