#!/usr/bin/env bash
exec "$(dirname "$0")/cargo.sh" build --profile "${PROFILE:-fast}" "$@"
