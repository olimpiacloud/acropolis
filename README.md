# Acropolis

Acropolis turns application source into an OCI image (or static artifacts) as fast as possible on a machine with nothing cached. It is a single Rust binary, `acro`, with no daemon.

```
acro build ./my-app -t registry.example.com/team/my-app:latest
acro plan ./my-app
acro inspect node:22-bookworm-slim
acro bench --apps express-api,go-api --tools docker,railpack,acro --runs 2 --cpus 0-1
acro e2e --examples ../railpack/examples
```

## How it works

- **Plan.** `acro plan` detects the language, framework, package manager and versions and produces a graph of steps. Every step has a canonical hash. Fetch steps have network access and verified outputs; build steps run without network in their own network namespace.
- **Content-addressed store.** Every download is verified while it streams (npm `sha512`, `Cargo.lock` checksums, `go.sum` `h1:` hashes, Node/Go/Rust release checksums, OCI digests) and stored once. Installs and image layers are produced from the store.
- **Parallel cold path.** Toolchains, packages and the base image are fetched at the same time. Large blobs are fetched as parallel range segments with hedged duplicates for slow segments; stalled streams are resumed with `Range`.
- **No unpacking of base images.** Base layers are copied registry to registry by digest without decompression. New layers are deterministic tars (mtime 0, uid/gid 0, stable order) compressed in parallel as independent zstd frames or gzip members.
- **Push.** Blobs are checked with `HEAD` before upload, so a base that already exists in the target registry is never uploaded again.

## Supported today

| Ecosystem | Detection | Install | Build | Runtime image |
|---|---|---|---|---|
| Node (npm, pnpm, bun, yarn 1, no lockfile) | package.json, lockfiles, engines, .nvmrc, mise, packageManager, devEngines | native installer from the store: npm/bun hoisted layout, pnpm isolated layout, yarn 1 hoisting, registry resolver | host toolchain from nodejs.org, scripts run without network | official `node:<v>-bookworm-slim` copied by digest; Next standalone, Nitro, SPA on Caddy |
| Bun | bun.lock, packageManager, start scripts | bun.lock installer | | Bun binary from the official npm package |
| Go | go.mod, toolchain, mise | modules from the proxy verified against go.sum | `go build` without network | `distroless/static` |
| Rust | Cargo.toml, rust-toolchain, rust-version | crates vendored from the store | `cargo build --locked --offline` without network | `distroless/cc` matching the host glibc |
| Python | uv, poetry, pdm, pipenv, pip, pyproject | inside `python:<v>-slim` with uv | | same base + `/app/.venv` |
| Ruby | Gemfile, .ruby-version | bundler inside `ruby:<v>-slim` | | same base + runtime libraries |
| Static sites, shell scripts | Staticfile, index.html, start.sh | | | Caddy, Debian slim |

## Repository layout

- `crates/acro-store` content-addressed store and integrity types
- `crates/acro-fetch` verified, segmented, hedged downloads
- `crates/acro-oci` deterministic tar and layers, registry client, image assembly
- `crates/acro-npm` lockfile parsers, installer, hoisting, registry resolver, lifecycle scripts
- `crates/acro-gomod`, `crates/acro-cargo`, `crates/acro-toolchain` language toolchains and dependencies
- `crates/acro-exec` execution backends (host, network namespace, image rootfs with overlayfs)
- `crates/acro-build` detection, plans and the step graph executor
- `crates/acro-cli` the `acro` binary, the cold benchmark and the end-to-end runner
- `bench/` benchmark apps, idiomatic Dockerfiles and results
- `docs/decisions.md` decision log with the measurements behind each choice

## License

Apache-2.0 OR MIT.
