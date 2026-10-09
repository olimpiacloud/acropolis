# Contributing

Thanks for your interest. Issues and PRs are welcome in Spanish or English; code, error messages and commits are in English.

## Requirements

- Linux x86_64 or arm64. Acropolis uses mount, PID and network namespaces and overlayfs, so it does not build or run on macOS or Windows (a VM or a `--privileged` container works).
- A recent stable Rust (the minimum version is the `rust-version` in `Cargo.toml`), with `rustfmt` and `clippy`.
- A C compiler (`cc`) for the native dependencies (zstd, liblzma, ring).
- For full builds: root. `acropolis plan` and most tests run without root; build steps inside images and the sandbox tests need root.
- For the e2e suite: Docker.

## Build and test

```
cargo build --bin acropolis           # target/debug/acropolis
cargo test --workspace                # unit tests
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test --workspace  # as root, like CI: includes the sandbox tests
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

CI runs the same plus `cargo deny check` (licenses, advisories and sources of dependencies, see `deny.toml`), the minimum Rust version, and the plan snapshots.

The scripts in `scripts/` (`build.sh`, `test.sh`, `e2e.sh`) wrap cargo in a systemd unit with a memory limit for the team's development VM; you don't need them to contribute.

## Plan snapshots

`tests/plans` stores the output of `acropolis plan --json` for the 131 [Railpack](https://github.com/railwayapp/railpack) examples. It is the fastest way to see whether a change alters detection:

```
git clone https://github.com/railwayapp/railpack ../acropolis-ext/railpack
git -C ../acropolis-ext/railpack checkout <RAILPACK_REF from .github/workflows/ci.yml>
cargo build --bin acropolis
BIN=target/debug/acropolis scripts/plans.sh            # compare
BIN=target/debug/acropolis scripts/plans.sh update     # regenerate, if the change is intended
```

Point `ACROPOLIS_EXT` to another directory if the clone is not next to the repo. If a PR changes plans, the `tests/plans` diff must be in the PR and the description must explain why.

## e2e

`scripts/e2e.sh` builds each Railpack example, runs it with `docker run`, and checks the output (it needs root, Docker and `target/fast/acropolis`, which `scripts/build.sh` or `cargo build --profile fast` produce). `--filter <example>` runs a single one. It is not required for a PR, but if you touch detection, dependency installation or layer assembly, running the affected examples saves a review round.

## Pull requests

- One change per PR, with a test when possible: a behavior test that fails without the change.
- The PR title follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) (`feat(npm): ...`, `fix(oci): ...`, `docs: ...`, `refactor!: ...` for breaking changes). PRs are squash-merged and the title becomes the commit message: the next version and `CHANGELOG.md` come from it. A CI check validates the title.
- If the change is a design decision with measurements (performance, image size, security), add it to [`docs/decisions.md`](docs/decisions.md).
- You don't need to sign commits or add `Signed-off-by`.

## Releases

They are published by [release-plz](https://release-plz.dev): on each merge to `main` it opens or updates a release PR with the new version and the changelog; merging it creates the `vX.Y.Z` tag and the GitHub release, and CI attaches the binaries for Linux x86_64 and arm64 (glibc 2.36 or newer), their checksums and the provenance attestation, and publishes the `ghcr.io/olimpiacloud/acropolis-builder` image.

The `release-plz` workflow uses a GitHub App token (with a `GITHUB_TOKEN` the release PR would not run CI and the release would not trigger `release.yml`). Until the App exists, the workflow is skipped. To enable it: create the App in the organization (no webhook, Contents and Pull requests read and write permissions, installed only on this repo), store its client ID as a repo variable and its private key as a secret of the `release` environment:

```
gh variable set RELEASE_PLZ_CLIENT_ID -R olimpiacloud/acropolis --body <client-id>
gh secret set RELEASE_PLZ_PRIVATE_KEY -R olimpiacloud/acropolis --env release < app-private-key.pem
```

The variable is set at repo level and not on the environment because the job's `if` is evaluated before entering the environment.

## Security

Vulnerabilities are not reported in issues: see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contribution is published under the project's same dual license, Apache-2.0 or MIT, at the user's option.
