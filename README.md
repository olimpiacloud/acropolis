# Acropolis

Acropolis turns application source into an OCI image (or static artifacts) as fast as possible on a machine with nothing cached. It is a single Rust binary, `acropolis`, with no daemon.

```
acropolis build ./my-app -t registry.example.com/team/my-app:latest
acropolis build ./my-app --oci image.tar --info info.json
acropolis plan ./my-app
acropolis inspect node:22-bookworm-slim
acropolis bench --apps express-api,go-api --tools docker,railpack,acropolis --runs 2 --cpus 0-1
acropolis e2e --examples ../railpack/examples
```

## How it works

- **Plan.** `acropolis plan` detects the language, framework, package manager and versions and produces a graph of steps. Every step has a canonical hash. Fetch steps have network access and verified outputs. Go and Rust builds run without network in their own network namespace; Node build scripts get network like `docker build` does (tools such as vite-plus, Prisma or Next fetch during the build), and `ACROPOLIS_HERMETIC_BUILD=1` runs them offline.
- **Content-addressed store.** Every download is verified while it streams (npm `sha512`, `Cargo.lock` checksums, `go.sum` `h1:` hashes, Node/Go/Rust release checksums, OCI digests) and stored once. Installs and image layers are produced from the store.
- **Parallel cold path.** Toolchains, packages and the base image are fetched at the same time. Large blobs are fetched as parallel range segments with hedged duplicates for slow segments; stalled streams are resumed with `Range`.
- **No unpacking of base images.** Base layers are copied registry to registry by digest without decompression. New layers are deterministic tars (mtime 0, uid/gid 0, stable order) compressed in parallel as independent zstd frames or gzip members.
- **Push.** Blobs are checked with `HEAD` before upload, so a base that already exists in the target registry is never uploaded again.

## Supported today

| Ecosystem | Detection | Install | Build | Runtime image |
|---|---|---|---|---|
| Node (npm, pnpm, bun, yarn 1, no lockfile) | package.json, lockfiles, engines, .nvmrc, mise, packageManager, devEngines | native installer from the store: npm/bun hoisted layout, pnpm isolated layout, yarn 1 hoisting, registry resolver | host toolchain from nodejs.org; build scripts run with network unless `ACROPOLIS_HERMETIC_BUILD=1` | official `node:<v>-bookworm-slim` copied by digest; Next standalone, Nitro, SPA on Caddy |
| Bun | bun.lock, packageManager, start scripts | bun.lock installer | | Bun binary from the official npm package |
| Go | go.mod, toolchain, mise | modules from the proxy verified against go.sum | `go build` without network | `distroless/static` |
| Rust | Cargo.toml, rust-toolchain, rust-version | crates vendored from the store | `cargo build --locked --offline` without network | `distroless/cc` matching the host glibc |
| Python | uv, poetry, pdm, pipenv, pip, pyproject, mise | inside `python:<v>` with uv; system libraries for psycopg, mysqlclient, cairo, poppler, ffmpeg | | `python:<v>-slim` + `/app/.venv`; free-threaded CPython via uv |
| Ruby | Gemfile, .ruby-version | bundler inside `ruby:<v>-slim` | | same base + runtime libraries |
| PHP | composer.json, index.php, Laravel | composer and extensions inside FrankenPHP | Vite assets with the app's package manager | `dunglas/frankenphp` |
| Elixir, Gleam | mix.exs, gleam.toml, .tool-versions | `mix deps.get` / `gleam export` in the official image | `mix release` | Debian slim matching the build image's glibc; `gleam:*-erlang-slim` |
| Java, .NET, Deno, C/C++ | pom.xml, build.gradle, *.csproj, deno.json, CMakeLists.txt, meson.build | inside the official SDK image | | JRE, ASP.NET runtime, Debian slim |
| Static sites, shell scripts | Staticfile, index.html, start.sh | | | Caddy, Debian slim |

`railpack.json` / `acropolis.json` configs are honored: custom steps with `deployOutputs` (like BuildKit, steps nothing in the image depends on are skipped), `packages` beyond the language runtimes (installed with mise, e.g. `pipx:httpie`, `jq`), `buildAptPackages`, `deploy.aptPackages`, `deploy.paths` and `deploy.inputs` from images or local files.

## Running in production

Run `acropolis` on builder machines as root (it needs mount namespaces and overlayfs). One process per build; builds can run concurrently against the same `--home`.

**Output.** `-t` pushes to a registry; `--oci FILE` writes an OCI image layout tar (what `buildctl --output type=oci` produces, loadable with `docker load`); `--info FILE` writes a JSON summary in the shape of Railpack's info file (`detectedProviders`, `resolvedPackages`, `metadata`, `success`, `error`), also on failure.

**Builder setup.** `acropolis prewarm` installs common toolchains ahead of time and precompiles the Go standard library (`--tools node:22,go:1.25,bun:latest,uv:latest,python:3.13`). Cap disk use with `acropolis gc --max-size 40G` on a timer, or set `ACROPOLIS_CACHE_MAX=40G` to collect after every build. GC evicts least recently used store blobs, toolchains, extracted rootfs and app caches; it never touches an app cache that a running build holds.

**Caches.** Every build of the same app reuses a persistent cache: Go build and module caches, the cargo target and vendor directories, `.next/cache`, `node_modules/.cache`, and `/root/.cache` (uv, pip, composer) for steps that run inside an image. Pass `-e ACROPOLIS_CACHE_KEY=<service id>` so the key survives path changes; without it the key is the app path. Concurrent builds of the same app run without the cache instead of sharing it. `ACROPOLIS_NO_CACHE=1` disables it. Base image tags are re-resolved after `ACROPOLIS_TAG_TTL` seconds (default 900, `0` always resolves).

**Isolation.** Build commands and install scripts run in a private mount namespace where the store, toolchains and extracted base images are read-only, with `CAP_SYS_ADMIN`, `CAP_SYS_CHROOT`, `CAP_MKNOD`, ptrace, module and raw I/O capabilities dropped and `no_new_privs` set. Steps that run inside a base image get a minimal `/dev` and a read-only `/sys`. Steps without network access get their own network namespace. Aborted or finished steps kill their whole process group. This protects the builder's shared caches from a malicious build; it is not a VM boundary, so run builders for different trust domains on separate machines.

**Limits.** `ACROPOLIS_STEP_TIMEOUT` and `ACROPOLIS_BUILD_TIMEOUT` (seconds). SIGTERM or SIGINT cancels the build and its processes. The first failing step cancels the rest.

**Results.** With `--events json`, stdout carries one JSON event per line (`step_started`, `step_finished`, `log`, `image_pushed`, `build_finished`, `build_failed`). Exit codes:

| code | class | meaning |
|---|---|---|
| 0 | | image built and pushed |
| 1 | user | the app's build or install script failed |
| 70 | internal | bug in acropolis |
| 75 | infra | registry, network, disk or OOM kill; safe to retry |
| 78 | config | the app cannot be built as configured (detection, config file, lockfile) |

**Reproducibility.** Lockfiles are honored exactly. An npm lockfile out of sync with `package.json` falls back to registry resolution like `npm install`; set `ACROPOLIS_STRICT_LOCKFILE=1` to fail instead. Environment values never appear in plans (`acropolis plan --json` shows `{env:NAME}` placeholders), so plans can be logged and diffed safely.

## Repository layout

- `crates/acropolis-store` content-addressed store and integrity types
- `crates/acropolis-fetch` verified, segmented, hedged downloads
- `crates/acropolis-oci` deterministic tar and layers, registry client, image assembly
- `crates/acropolis-npm` lockfile parsers, installer, hoisting, registry resolver, lifecycle scripts
- `crates/acropolis-gomod`, `crates/acropolis-cargo`, `crates/acropolis-toolchain` language toolchains and dependencies
- `crates/acropolis-exec` execution backends (host, network namespace, image rootfs with overlayfs)
- `crates/acropolis-build` detection, plans and the step graph executor
- `crates/acropolis-cli` the `acropolis` binary, the cold benchmark and the end-to-end runner
- `bench/` benchmark apps, idiomatic Dockerfiles and results
- `docs/decisions.md` decision log with the measurements behind each choice

## License

Apache-2.0 OR MIT.
