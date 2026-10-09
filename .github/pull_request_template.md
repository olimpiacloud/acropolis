<!-- The title follows Conventional Commits (feat:, fix:, docs:, refactor!: ...): it becomes the commit message and builds the changelog. -->

## What changes and why

## How it was tested

<!-- Tests added, `scripts/plans.sh`, e2e examples run. -->

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] If detection changes, the `tests/plans` diff is included and explained.
- [ ] If it is a design decision with measurements, it is in `docs/decisions.md`.
