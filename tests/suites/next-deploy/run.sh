#!/usr/bin/env bash
# Next.js deploy tests (NEXT_TEST_MODE=deploy) against Acropolis: the harness creates each test app, deploy.sh builds
# it with Acropolis and runs it with docker, and the tests drive it over HTTP and Playwright.
#
#   tests/suites/next-deploy/run.sh 16.4.0 [test files or dirs...]   (default: SAMPLE=N files of test/e2e/app-dir)
#
# The Next.js checkout (sparse: test/, the root and packages/next/package.json) lives in
# $ACROPOLIS_EXT/next-deploy/next-<version>; packages/next/dist comes from the published tarball of the same version
# instead of building the monorepo, and test apps install `next@<version>` from npm (NEXT_TEST_VERSION).
# Next 15 has no custom deploy scripts: next15-custom-deploy.patch adds the 16.x contract to its harness.
# Results: tests/results/next-deploy-<version>-<rev>-<stamp>.jsonl (one line per test file, like the Railpack e2e)
# with the per-test JUnit reports next to it.
set -euo pipefail
cd "$(dirname "$0")/../../.."
repo=$PWD
version="${1:?usage: run.sh <next version> [tests...]}"
shift
ext="${ACROPOLIS_EXT:-$repo/../acropolis-ext}"
src="$ext/next-deploy/next-$version"
suite="$repo/tests/suites/next-deploy"
major="${version%%.*}"

if [ ! -d "$src/.git" ]; then
  mkdir -p "$ext/next-deploy"
  git clone -q --depth 1 --branch "v$version" --filter=blob:none --sparse https://github.com/vercel/next.js "$src"
  # The root's devDependencies name workspace packages (`workspace:*`) and vendored dirs (`file:`) that pnpm 9
  # (Next 15) wants on disk even with `--filter .`.
  git -C "$src" sparse-checkout set --no-cone '/*' '!/*/' '/scripts/' '/patches/' '/test/' '!/test/*/' \
    '/test/lib/' '/test/__mocks__/' '/test/e2e/app-dir/' '/packages/*/package.json' \
    '/packages/next/src/compiled/react-server-dom-*/' '/.github/actions/next-stats-action/'
  if [ "$major" = 15 ]; then
    git -C "$src" apply "$suite/next15-custom-deploy.patch"
  fi
fi
cd "$src"
pm=$(node -p 'require("./package.json").packageManager')
if [ ! -d node_modules ]; then
  # Only the root's test tooling: the other workspace packages are not checked out.
  COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack "$pm" install --filter . --frozen-lockfile --ignore-scripts >&2 \
    || COREPACK_ENABLE_DOWNLOAD_PROMPT=0 corepack "$pm" install --filter . --no-frozen-lockfile --ignore-scripts >&2
fi
if [ ! -f packages/next/dist/server/next.js ]; then
  # The published package and its runtime dependencies (@next/env, @next/swc-*, styled-jsx, ...) replace the
  # workspace package the harness and next/jest load, so the monorepo never has to be built.
  tmp=$(mktemp -d)
  (cd "$tmp" && npm pack -q "next@$version" >/dev/null && tar -xzf next-*.tgz)
  # Outside the checkout: npm would take the monorepo root (and its `workspace:` specs) for the project.
  (cd "$tmp/package" && npm install -q --omit=dev --omit=peer --no-package-lock --ignore-scripts --no-audit --no-fund >&2)
  rm -rf packages/next
  mv "$tmp/package" packages/next
  rm -rf "$tmp"
fi
# jest's `roots` name workspace packages that are not checked out.
{ grep -o "<rootDir>/\.\./packages/[^']*" jest.config.js || true; } | sed 's#<rootDir>/\.\./##' | xargs -r mkdir -p
[ -e node_modules/next ] || ln -s ../packages/next node_modules/next
# The browser build this checkout's Playwright pins (a no-op when it is already there).
node_modules/.bin/playwright install chromium-headless-shell >&2 2>/dev/null || node_modules/.bin/playwright install chromium >&2
test_files() { find "$@" \( -name '*.test.ts' -o -name '*.test.tsx' -o -name '*.test.js' \) | LC_ALL=C sort; }
if [ $# -eq 0 ]; then
  # A deterministic spread over test/e2e/app-dir: every k-th test file of the sorted list.
  mapfile -t all < <(test_files test/e2e/app-dir)
  n="${SAMPLE:-40}"
  step=$(( (${#all[@]} + n - 1) / n ))
  set --
  for ((i = 0; i < ${#all[@]}; i += step)); do set -- "$@" "${all[i]}"; done
else
  # Directories become their test files: results are per file.
  mapfile -t all < <(test_files "$@")
  set -- "${all[@]}"
fi

rev=$(git -C "$repo" rev-parse --short HEAD)
git -C "$repo" diff --quiet HEAD -- crates Cargo.toml Cargo.lock || rev="$rev-dirty"
stamp=$(date +%Y%m%d-%H%M%S)
out="$repo/tests/results/next-deploy-$version-$rev-$stamp.jsonl"
logs="${out%.jsonl}.logs"
mkdir -p "$logs"
bin="${ACROPOLIS_BIN:-$repo/target/fast/acropolis}"
cp "$bin" "$logs/acropolis"
export ACROPOLIS_BIN="$logs/acropolis"
export NEXT_DEPLOY_BUILDS="$logs/builds.tsv"
export NEXT_DEPLOY_BASELINE_ENV="$logs/baseline.env"
env -0 > "$NEXT_DEPLOY_BASELINE_ENV"

export NEXT_TEST_MODE=deploy
export NEXT_TEST_VERSION="$version"
export NEXT_TEST_DEPLOY_SCRIPT_PATH="$suite/deploy.sh"
export NEXT_TEST_DEPLOY_LOGS_SCRIPT_PATH="$suite/logs.sh"
export NEXT_TEST_CLEANUP_SCRIPT_PATH="$suite/cleanup.sh"
export NEXT_E2E_TEST_TIMEOUT="${NEXT_E2E_TEST_TIMEOUT:-240000}"
export NEXT_TELEMETRY_DISABLED=1
export NEXT_JUNIT_TEST_REPORT=true
export NEXT_TEST_JOB=1
# Next 15's run-tests.js stops at the first failed file otherwise.
export NEXT_TEST_CONTINUE_ON_ERROR=1
[ -f test/deploy-tests-manifest.json ] && export NEXT_EXTERNAL_TESTS_FILTERS=test/deploy-tests-manifest.json
# Next 16 builds with Turbopack by default, Next 15 with webpack.
if [ "$major" -ge 16 ]; then export IS_TURBOPACK_TEST=1; else export IS_WEBPACK_TEST=1; fi
# jest's rootDir is test/, so the JUnit reports land in test/*-junit-report.
rm -rf test/test-junit-report test/turbopack-test-junit-report
printf '%s\n' "$@" > "$logs/tests.txt"
echo "next $version: ${#} test files, results in $out" >&2
set +e
node run-tests.js --type e2e -c "${CONCURRENCY:-2}" --retries 0 "$@" > "$logs/run-tests.log" 2>&1
code=$?
set -e
cp -r test/test-junit-report test/turbopack-test-junit-report "$logs/" 2>/dev/null || true
python3 "$suite/results.py" "$logs" "$out" "$@"
echo "run-tests exit $code; $(wc -l < "$out") test files in $out" >&2
