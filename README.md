# Acropolis

![Acropolis: an acropolis built up in layers facing the bay of Buenos Aires](docs/assets/acropolis-banner.jpg)

[![CI](https://github.com/olimpiacloud/acropolis/actions/workflows/ci.yml/badge.svg)](https://github.com/olimpiacloud/acropolis/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/olimpiacloud/acropolis)](https://github.com/olimpiacloud/acropolis/releases/latest)
[![License: Apache-2.0 or MIT](https://img.shields.io/badge/license-Apache--2.0%20or%20MIT-blue)](#license)

Acropolis takes an app's source code and returns an OCI image ready to run, with no Dockerfile and no daemon. It detects the language and framework, downloads verified toolchains and dependencies, compiles, and assembles the image. It is a single Rust binary (`acropolis`) and it is the builder behind [Olimpia](https://olimpia.dev).

```
acropolis build ./my-app -t registry.example.com/team/my-app:latest
acropolis build ./my-app --oci image.tar --info info.json
acropolis plan ./my-app
```

We wrote it because on a PaaS every build starts on an empty machine, and the tools available there were slow, heavy, or produced huge images.

## How we make sure it works

We don't trust our own test apps: we use other builders' tests as they publish them. Each case builds the image with Acropolis, runs it with `docker run`, and checks the output or an HTTP request, with the same timeouts as the original.

| suite | what it is | result |
|---|---|---|
| [Railpack](https://github.com/railwayapp/railpack/tree/main/examples) examples | 131 apps and 156 cases: Node (npm, pnpm, yarn, bun, Next, Nuxt, Astro, SvelteKit, Remix, Angular, Nx, Turborepo), Python (pip, uv, poetry, pdm), Go, Rust, Ruby/Rails, PHP/Laravel, Java, .NET, Elixir, Gleam, Deno, static sites and scripts | **154 pass**; the other 2 are for arm64 and are skipped on x86 |
| Next.js and Turbopack (`tests/suites/next`) | Next 15 with `--turbopack`, Next 16 with Turbopack by default, `--webpack`, `output: standalone`, `output: export`, `next/image`, file reads at runtime and a Turborepo monorepo; with npm, pnpm, yarn and bun | **8/8** |
| [Nixpacks](https://github.com/railwayapp/nixpacks) tests (`scripts/nixpacks-suite.py`) | 73 cases converted from `tests/docker_run_tests.rs` | **37 pass**; the rest are languages Railpack doesn't support either (Clojure, Crystal, Dart, Haskell, Scala, Scheme, Swift, Zig), Nixpacks-specific configs (`nixpacks.toml`, `NIXPACKS_*`), or apps that expect the compiler inside the final image |
| Plan snapshots (`tests/plans`) | `acropolis plan --json` for the 131 Railpack examples | any detection change shows up as a diff before anything is built |
| Unit tests | package installation, layers, sandbox, registry, paths that escape the app, errors | 128 |

These suites found bugs our own apps would never have shown: yarn v1 installing binaries for every platform (177 MB of `sharp` for darwin, windows and arm in a linux image), `node_modules` of pnpm workspace members not reaching the image, Next 15 needing `typescript` at runtime to read `next.config.ts`, `patchedDependencies` patches not being applied, duplicate hardlinks inflating layers, Go without `go.mod`, and Java with an old Gradle.

Images are tested with `docker pull` from a local registry; with `E2E_OCI=1` the harness also loads the `--oci` tar with `docker load`, which is how Olimpia consumes them.

## What we saw nobody solving

- **Cold builds are slow.** In our benchmark Railpack takes 54 to 170 s because it first downloads its builder image, and BuildKit does not retry a download that hangs: we saw a 68 MB layer download at 200 KB/s for 10 minutes. Docker is faster, but only if someone wrote a multi-stage Dockerfile by hand.
- **Images are bloated.** Railpack leaves devDependencies, the cache and its toolchain manager in the image: a sample Next app weighs 345 MB against 105 MB with a well-made Dockerfile.
- **A lot of memory for little work.** Railpack uses more than 2.9 GB of RAM to build an Express server with 7.5 MB of dependencies.
- **Rebuilds reuse nothing.** With Docker, changing one line of a Go app recompiles everything (41 s).
- **No cache survives on an ephemeral builder.**

## What came out of it

Cold and rebuild benchmark with 2 CPUs, all three tools on the same day under the same conditions, median of 2 runs. Docker uses an idiomatic multi-stage Dockerfile for each app (`bench/dockerfiles`).

| app | cold time (Docker / Railpack / **Acropolis**) | image (Docker / Railpack / **Acropolis**) | peak RAM (Docker / Railpack / **Acropolis**) |
|---|---|---|---|
| Express | 16.1 s / 53.7 s / **6.6 s** | 81.2 / 147.5 / **54.6 MB** | 562 / 2955 / **83 MB** |
| Go | 77.9 s / 85.3 s / **41.7 s** | 4.3 / 40.8 / **4.2 MB** | 2221 / 2944 / **1088 MB** |
| Rust | 75.3 s / 112.4 s / **48.2 s** | 10.0 / 38.0 / **11.4 MB** | 2910 / 4523 / **1509 MB** |
| Vite + React | 23.0 s / 55.3 s / **5.2 s** | 26.4 / 54.7 / **25.0 MB** | 933 / 2863 / **594 MB** |
| Vite + MUI | 48.3 s / 77.8 s / **8.4 s** | 26.5 / 54.7 / **25.0 MB** | 1329 / 3262 / **898 MB** |
| TanStack Start | 26.8 s / 78.1 s / **8.1 s** | 80.1 / 203.0 / **53.6 MB** | 1242 / 3575 / **960 MB** |
| Next 15 | 118.1 s / 170.5 s / **47.5 s** | 105.4 / 345.6 / **69.8 MB** | 3075 / 4560 / **1936 MB** |

Rebuild after changing one file: Go 41 s / 5.8 s / **1.1 s**, Rust 49 s / 15 s / **6.5 s** and Next 71 s / 81 s / **24.8 s**.

Peak RAM includes page cache. Anonymous memory, which cannot be reclaimed, is on par with Docker on cold builds and below it on rebuilds: most of it is each language's compiler, not Acropolis.

On a real monorepo, olimpia-cloud (bun workspace; TanStack Start/Nitro web app and an Axum + sqlx API), against its own production Dockerfiles, 4 CPUs, median of 2 runs:

| app | cold (Docker / **Acropolis**) | rebuild, one file changed | image, compressed / unpacked | peak RAM, cold / rebuild |
|---|---|---|---|---|
| web | 53.3 s / **42.4 s** | 26.0 s / **26.9 s** | 66.4 / 259.2 MB vs **57.1 / 224.0 MB** | 3077 / 3187 MB vs **3708 / 1875 MB** |
| api | 240.5 s / **228.0 s** | 109.7 s / **115.3 s** | 23.2 / 91.1 MB vs **24.5 / 95.2 MB** | 3988 / 3918 MB vs **3483 / 1781 MB** |

Here the compilers dominate (`vp build` ~20 s, `cargo build --release` of the API ~190 s), and those Dockerfiles are already well tuned (cache mounts, distroless/alpine runtimes), so the gap is small. Acropolis builds them with no Dockerfile: it only needed an `acropolis.json` input to copy `regctl` into the API image, as the Dockerfile does. The API image runs on `distroless/cc-debian13` because the benchmark host has glibc 2.39; on the Debian 12 builder image it is `cc-debian12`, the Dockerfile's base.

What mattered most:

- **No builder image.** Node, Go and Rust are downloaded from their official sources with a checksum and run directly; dependencies (npm, pnpm, yarn, bun, Go modules, crates) are installed by Acropolis from the lockfiles, verified while they download.
- **Downloads that don't hang.** Large blobs are downloaded in parallel segments, and if one falls behind a duplicate is started and the first to finish wins.
- **The base is never decompressed.** Base image layers are copied registry to registry by digest; only the new layers are compressed, in parallel.
- **Small runtimes.** Node and Bun run on distroless with a minimal shell when nothing from Debian is needed, and Next 15+ apps without `output` configured are built as standalone.
- **Portable caches.** Go, Cargo, `.next/cache` and the rest live in a per-app cache that is exported and imported as a `tar.zst` (`acropolis cache export|import`), to keep it between ephemeral builders.

The measurements, and what we tried and discarded, are in [`docs/decisions.md`](docs/decisions.md).

## What it supports

| ecosystem | image runtime |
|---|---|
| Node: npm, pnpm, yarn 1, yarn berry, bun; Next, Nuxt, Astro, SvelteKit, Remix, React Router, TanStack Start, Angular, Vite, Nx, Turborepo | distroless with a minimal shell when possible; otherwise `node:<version>-bookworm-slim`. Next standalone, Nitro, SPA on Caddy |
| Bun | `distroless/cc` + bun, or Debian slim |
| Go | `distroless/static` |
| Rust | `distroless/cc` |
| Python: uv, poetry, pdm, pipenv, pip | `python:<version>-slim` |
| Ruby, PHP/Laravel, Java, .NET, Elixir, Gleam, Deno, C/C++ | the official slim image of each one |
| Static sites and scripts | Caddy, Debian slim |

It honors `railpack.json` and `acropolis.json`: custom steps, extra packages, `buildAptPackages`, `deploy.aptPackages`, `deploy.paths` and `deploy.inputs`.

## Installation

Acropolis runs on Linux x86_64 and arm64 with glibc 2.36 or newer (Debian 12, Ubuntu 24.04 or newer). Each [release](https://github.com/olimpiacloud/acropolis/releases) ships the binary, its checksums, and a provenance attestation signed by GitHub Actions:

```
v=v0.1.0 target=x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu
gh release download "$v" -R olimpiacloud/acropolis -p "acropolis-$v-$target.tar.gz" -p SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify "acropolis-$v-$target.tar.gz" -R olimpiacloud/acropolis
tar xzf "acropolis-$v-$target.tar.gz" && sudo install "acropolis-$v-$target/acropolis" /usr/local/bin/
```

The builder image (see below) is published as `ghcr.io/olimpiacloud/acropolis-builder:<version>`. From source: `cargo build --release --bin acropolis` leaves the binary in `target/release/acropolis`.

## In production

- **One build per container.** Acropolis runs as root because it uses mount and PID namespaces and overlayfs. Each build step runs in its own PID namespace (it can't see Acropolis or its variables), with the whole filesystem read-only except its working directory, its app's cache and `/tmp`, read-only `/proc/sys` and `/sys`, a minimal `/dev`, only the capabilities of an unprivileged container, a seccomp filter modeled on Docker's default profile (no new namespaces, mounts, ptrace, kernel keyring or BPF), without the Docker credential directories, and with its own network namespace if it doesn't need network. What Acropolis processes as root (config, lockfiles, patches, image layers, caches, config paths) is validated so it cannot write or read outside its tree. Still, this is not a VM boundary: for builds from different customers, use one throwaway container per build.
- **Builder network.** Steps with network (installs with scripts, `next build`) can talk to any host the container can reach. Block the cloud metadata endpoint (`169.254.169.254`) and the internal network from outside; Acropolis only rejects private registries or `http://` for what it downloads itself (`ACROPOLIS_ALLOW_PRIVATE_REGISTRY=1` allows them).
- **Operator settings.** `ACROPOLIS_CACHE_KEY`, `ACROPOLIS_CACHE_MAX`, `ACROPOLIS_BUILD_TIMEOUT` (1 h by default), `ACROPOLIS_STEP_TIMEOUT` and `ACROPOLIS_BUILD_ID` are read only from the process environment; if they come through `-e` or the repo config they are ignored.
- **Output.** `-t` pushes to a registry, `--oci` writes a tar loadable with `docker load`, and `--info` writes a JSON summary. With `--events json` each stderr line is an event with a schema version and `build_id`.
- **Exit codes.** 0 ok, 1 the app build failed, 70 Acropolis bug (including a panic), 75 infrastructure problem (retryable; includes SIGTERM/SIGINT and steps killed with SIGKILL), 78 the app cannot be built as configured (includes paths that escape the app and private registries).
- **Builder image.** [`deploy/builder`](deploy/builder) has a Debian 12 image with native build tools and prewarmed toolchains.

## Development

[`CONTRIBUTING.md`](CONTRIBUTING.md) explains how to build and test with plain `cargo`, how to run the plan snapshots and the e2e, and how PRs and releases work. On the team VM we use these shortcuts:

```
scripts/build.sh               # builds target/fast/acropolis
scripts/test.sh --workspace    # unit tests
scripts/plans.sh               # compares the plans of the Railpack examples
scripts/e2e.sh                 # runs the Railpack examples (ACROPOLIS_EXT points to the clone)
scripts/bench.sh               # benchmark against Docker and Railpack
```

The scripts expect, next to the repo, an `acropolis-ext` directory with the Railpack clone and its binary (or the path in `ACROPOLIS_EXT`). Vulnerabilities are reported privately: see [`SECURITY.md`](SECURITY.md).

## License

At your option, [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT). Unless stated otherwise, any contribution is published under those same two licenses.
