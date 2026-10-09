# Decision log

Each entry: what was tried, what number it gave, what was kept and what was discarded. Measurements are from the development VM (5 vCPU AMD EPYC-Rome, 7.7 GB, Ubuntu 24.04, glibc 2.39) unless stated otherwise.

## D1. Node runtime for servers without a build: copy `node:<version>-bookworm-slim` by digest

- Measured: the official Node tarball ships 67 MB of `include/` that is only useful for node-gyp. Downloading + decompressing `.tar.xz` costs 3.9 s wall time and 3.4 s CPU; `.tar.gz` 2.1 s and 1.7 s.
- Recompressing only the `node` binary (118 MB) as a layer: gzip-6 7.3 s, zstd-3 0.9 s (0.37 s with 4 threads), zstd-1 0.64 s.
- Copying the official image registry to registry spends no CPU (0.8 CPU-s for the whole Express build).
- Decision: for Node apps without a build step, the base is the official image by version tag (`22` → `node:22-bookworm-slim`, exact → `node:23.5.0-bookworm-slim`). Ranges (`>=18`) are resolved against `nodejs.org/dist/index.json`.
- Pending: measure the `distroless/cc` + `node` layer alternative, built from the already downloaded toolchain, for apps with a build (SSR), where the toolchain is downloaded anyway.

## D2. Segmented range downloads with duplication of the lagging segment

- Measured: registry CDNs deliver 5–14 MB/s per stream from this VM; nodejs.org ~34 MB/s; npm 60–74 MB/s. A `curl` to nodejs.org hung for 10 minutes without bytes; dl.google.com stalled at 96%. In a Railpack run, a 68 MB layer of its builder image downloaded at ~200 KB/s for 10 minutes (BuildKit does not retry slow streams).
- Decision: blobs ≥ 12 MB are downloaded in 8 MB segments, 6 in parallel over independent HTTP/1.1 connections (with HTTP/2 the duplicates would travel over the same slow connection). If the segment that blocks in-order output takes longer than max(2.5 s, 2.5 × median), a duplicate is started and the first to finish wins. Streams without bytes for 6 s are cut and resumed with `Range`.
- Result: copying `node:22-bookworm-slim` (80 MB) from Docker Hub to the target registry went from 12.2 s to 2.9 s.

## D3. Start copying the base as soon as the platform manifest arrives

- Resolving a base takes three serial requests (index, manifest, config). On mirror.gcr.io each one takes ~1.2 s of server time even by digest; on Docker Hub ~0.3–0.5 s.
- Decision: layer copying starts after the manifest, and the config is downloaded in parallel. The Docker Hub and ghcr.io token is also requested without waiting for the 401.
- Express cold: 6.0 s → 3.2–4.1 s with Docker Hub directly.

## D4. Layers are written as fragments compressed in parallel

- A layer is a concatenation of independent gzip members (or zstd frames) of ~1 MB, compressed in parallel with rayon. It is valid (multi-member) gzip/zstd and deterministic; `diff_id` is computed over the concatenated tar.
- Production `node_modules` is written directly from the store tarballs, without unpacking to disk: Express (76 packages, 7.5 MB uncompressed) in 0.05 s.

## D5. Build steps without network via a namespace

- `unshare(CLONE_NEWNET)` (or `CLONE_NEWUSER|CLONE_NEWNET` without root) with loopback brought up. If the host cannot create namespaces, the build fails unless `--hermetic=off`.
- Explicit exception, visible in the plan: `next build` runs with network (Next downloads Google Fonts at build time). The plan marks it `build+net` and emits a warning.

## D6. Go: modules verified against `go.sum` and a module cache assembled by Acropolis

- The `.zip` and `.mod` files are downloaded from the proxy in parallel, the `h1:` hash (dirhash) is verified against `go.sum`, and the module cache layout (`<mod>@<v>/` + `.ziphash`) is written. `go build` runs with `GOPROXY=off`, `GOFLAGS=-mod=readonly`, without network.
- `src/cmd`, `test`, `api`, `doc`, `misc`, `*_test.go` and `testdata` are skipped from the Go tarball: ~120 MB less written out of 243 MB.
- Measured (5 vCPU): toolchain 2.4 s, 34 modules 2.8 s, `go build` 21.7 s. The compiler dominates, as the document anticipated.

## D7. DNS inside BuildKit in the benchmark

- The host `resolv.conf` points to the systemd-resolved stub (127.0.0.53). BuildKit `RUN`s on the bridge network can't reach it: `npm ci` hung for 353 s and mise failed resolving DNS.
- Neutralization: `bench/buildkitd.toml` sets `[dns] nameservers`. The repo ships public resolvers (1.1.1.1, 8.8.8.8); set them to the host's upstreams (`resolvectl dns`) when benchmarking. Acropolis uses the host resolver. This is not a product difference.

## D8. Docker Hub directly in the benchmark, no mirror

- mirror.gcr.io adds ~1.2 s per request; Docker Hub directly has lower latency. The anonymous limit (100 pulls/h) is enough for 5 apps × 3 tools × 2 runs.
- Railpack pulls its images from ghcr.io, which is not affected.

## D9. Rust: toolchain from the channel manifest, vendored crates, and base chosen by host glibc

- The `rustc`, `cargo` and `rust-std` components come from `static.rust-lang.org`, verified with the sha256 of the channel manifest, in parallel and streaming. `rustc` as `.tar.xz` is 84 MB against 141 MB as `.tar.gz`; decoding xz costs ~7.9 CPU-s, but the CPU is idle while the toolchain downloads, so fewer bytes are preferred.
- Crates come from `static.crates.io`, verified with the `checksum` in `Cargo.lock`, vendored with `.cargo-checksum.json` and with `source.crates-io` replaced. `cargo build --locked --offline` runs without network, with `CARGO_TARGET_DIR` outside the repo.
- The binary links against the host glibc: the runtime base is chosen with a glibc ≥ the host's (`distroless/cc-debian12` up to 2.36, `cc-debian13` up to 2.41); if the host is newer, the build fails explicitly.
- Pending: use `zig cc` as the linker to pin the target glibc version and not depend on a `cc` on the host.

## D10. Railpack-compatible version policy

- Ranges are converted to a "fuzzy" version the same way Railpack does: `>=20.0.0` → `20`, `^18.2` → `18`, `~22.1` → `22.1`, `20.x` → `20`, and the latest patch is taken. For Node this allows picking the `node:<v>-bookworm-slim` tag without downloading `index.json`.
- Reason: the `node-oldest` example (`engines: >=20.0.0`) expects Node 20; with "latest version that satisfies the range" it got 26.

## D11. Direct start with the package manager environment

- Railpack starts with `npm run start`/`pnpm run start`. Acropolis runs the command directly (one process fewer, less memory, correct signal handling) but sets `npm_config_user_agent`, `npm_lifecycle_event` and `npm_package_name`, and if the project uses pnpm it adds the CLI (`pnpm` from the npm registry, verified) in `/usr/local`.

## D12. Docker Hub fallback to a verified mirror

- During testing this VM lost connectivity to AWS us-east-1 (S3, STS and `registry-1.docker.io` hung on SYN; CloudFront and Cloudflare responded). If the connection to Docker Hub (registry or `auth.docker.io`) fails, Acropolis switches to `mirror.gcr.io`. This is safe because every manifest and blob is verified by digest. Connection timeout: 5 s.

## D13. Steps off the host: image rootfs with overlayfs

- For languages without relocatable binaries (Ruby, Python with extensions, PHP, Java, .NET, Elixir) and for apt packages, steps run inside the rootfs of the official image: layers unpacked once as `lowerdir`, one `upper` per step, mount/UTS/IPC namespaces (and network if it is a build step). The `upper` becomes an OCI layer by translating overlay whiteouts to OCI whiteouts.
- Cost: unpacking the base image (exactly what Node/Go/Rust avoid). That is why it is only used where there is no alternative.

## D14. Own bundler for Vite SPAs: lazy installation with a per-package index

- Rolldown 1.2.12 as the engine (crate from crates.io), with our own `resolve_id`/`load` plugin:
  - A "bare" import looks up the package in the lockfile plan (Node resolution through the importer's `node_modules` chain), fetches that tarball from the store and indexes it in memory. The package is not extracted: `exports` (conditions `import`/`module`/`browser`/`production`/`default`, or `require`), `imports` (`#`), `browser`, `module`, `main` and extensions are resolved against the index, without syscalls, and only the resolved file is written to disk (plus the `package.json` files along the path, for `type`).
  - When a package is indexed, its `dependencies`/`peerDependencies` are prefetched in the background to break the sequential "waves" of the graph.
  - `sideEffects` from `package.json` is passed to Rolldown for tree-shaking.
  - CSS: Rolldown no longer bundles CSS. Imported `.css` files are collected in the module order of the chunks and processed with lightningcss (minify, inline `@import`, `url()` to `/assets/<name>-<hash>`).
  - Assets as in Vite: a hashed file under `/assets/`, or a data URL if smaller than 4 KB; absolute imports resolve against `public/`.
  - If our resolver can't handle a case (a `browser` field as an object, etc.), it extracts the whole package and lets Rolldown resolve.
- It is enabled only if `vite.config` has no plugins other than `@vitejs/plugin-react(-swc)`, there is no PostCSS/Tailwind, and the build script is `[tsc ... &&] vite build`. `ACROPOLIS_BUNDLER=vite` forces the Vite path.
- The type check (`tsc -b`) is kept: it runs in parallel with the bundle over the full installation; the build fails if it fails.
- Measured (5 vCPU, cold):
  - Vite + React: the bundle downloads 3 of 174 packages (1.4 MB) and takes 0.43–0.49 s; total build 3.5–4.0 s against 9.1 s with Vite.
  - React + MUI: 85 of 231 packages (7.1 MB) in 1.4–1.5 s; total build 7.1 s against 15.6 s. Before the per-file index the bundle took 4.7 s: `@mui/icons-material` is 175 MB and ~49,000 files on disk to use 2 icons.
  - Equivalence: same DOM (normalizing hashes and whitespace between tags), pixel-identical screenshots and no console errors in headless Chromium (`tests/render/compare.mjs`).

## D15. VM resources during testing

- Compiling Rolldown in parallel with the e2e triggered the OOM killer (7.7 GB). The e2e runs with 2 jobs and each build uses its own rootfs directory, which is deleted when the case finishes (without that, the shared rootfs reached 6.7 GB and filled the disk).

## D16. Node runtime on distroless with a busybox shell

- Measured (registry, compressed, amd64): `node:24-bookworm-slim` 80.8 MB; `gcr.io/distroless/nodejs24-debian12` 52.7 MB; the `:debug` variant 53.5 MB (it adds a 740 KB busybox layer, identical in every distroless `:debug` image). `nodejs24-debian13` 55.3 MB, `nodejs26-debian13` 60.2 MB.
- Decision: the base is `gcr.io/distroless/nodejs<major>-debian12:debug` (or `-debian13` if the production scripts compiled against a newer glibc), with our own layer of three symlinks: `/bin/sh → /busybox/sh`, `/usr/bin/env → /busybox/env` and `/usr/local/bin/node → /nodejs/bin/node`. Without it, the `#!/usr/bin/env node` scripts in `node_modules/.bin` and `child_process.exec` fail.
- Only if the requested version floats (`N`, `N.x`, `lts`, `latest`, `^N`, `>=N`): distroless publishes the latest version of each major, but late. Measured on 2026-10-09: distroless had node 22.22.0 (nodejs.org latest 22.23.3), 24.14.0 (latest 24.21.0, seven minors behind) and 26.11.0 (26.11.1), so the lag can't be bridged with a fixed margin, and the image carries no metadata with its node version. Allowed majors: 22, 24 and 26.
- A pinned version (`22.2.0`, `>=22.18.0`, `^24.15.0`) on a server that is built (Nitro, Next standalone, `node <file>` after a build) runs on `distroless/cc-debian12:debug` (or `-debian13`) plus a layer with the `node` binary of the build toolchain, so the runtime is exactly the node it was built with (this was the pending item of D1). Measured with olimpia-cloud's web app (TanStack Start/Nitro, `engines.node >=22.18.0`): 84.5 MB compressed / 343.7 MB unpacked on `node:22-bookworm-slim`, 57.1 MB / 224.0 MB now; its Dockerfile on `node:24-alpine` gives 66.4 MB / 259.2 MB. Servers without a build step keep `node:<v>-bookworm-slim`, where no toolchain is downloaded.
- It falls back to Debian slim if there are install scripts in production (except Next standalone and Nitro, which don't install production dependencies separately), a start through a shell or through npm/pnpm/yarn, puppeteer or playwright, deploy apt packages, custom steps, extra mise tools, or `ACROPOLIS_RUNTIME_BASE=debian`. The resolution happens at build time: if the major has no distroless image, `node:<version>-bookworm-slim` is used and the symlink layer stays empty.
- Bun: `distroless/cc-debian12:debug` (9.9 MB) + a bun layer, with no `node` at all (Railpack requires this in `node-bun-no-deps`), only when the start is `bun <file>` or a `bun run X` that ends in `bun <file>`. A `bun run start` that calls `node` stays on Node.
- Risks reviewed: Prisma needs the shell (it runs it to detect OpenSSL) and distroless ships libssl3; sharp ships its own libs; there is no `node` user (uid 1000) and no `bash`/`npm` for `docker exec`.

## D17. Next without `output` is built as standalone

- `next start` needs the full production `node_modules` (in `node-next` the image weighed 162.9 MB). Next 14, 15 and 16 read `output: process.env.NEXT_PRIVATE_STANDALONE ? 'standalone' : undefined` as the default, so setting that variable in the build step is enough.
- Besides the standalone output, the image carries the app files without `node_modules`, `.next`, `.git` or `public` (`layer-files` layer): `server.js` does a `chdir` to its directory, so reads relative to `cwd` keep working (markdown content, i18n configs).
- It is only forced with Next ≥ 15 (or 13/14 with `sharp`), a build that is exactly `next build [--flags]`, a start that is `next start` with optional `-p`/`-H`, no `output`, `distDir`, `RuntimeConfig` or `PHASE_` in the config, and no dependencies that nft doesn't trace (`dd-trace`, `newrelic`, `@opentelemetry/auto-instrumentations-node`, `@sentry/profiling-node`, `geoip-lite`, `pdfkit`, `@grpc/proto-loader`, `pino`, `next-i18next`). Yarn PnP and `bun.lockb` are excluded. `ACROPOLIS_NEXT_STANDALONE=0` turns it off.
- Good side effect: Next 15 with `next.config.ts` no longer needs `typescript` at runtime, because the config is serialized into `server.js`. For the `next start` path, `typescript` is kept in the production dependencies on Next < 16.
- In monorepos the standalone output is nested (`.next/standalone/<member>/server.js`): the image keeps that structure and the workdir is `/app/<member>`.

## D18. Operator knobs separated from the app's

- Previously, `ACROPOLIS_CACHE_KEY`, `ACROPOLIS_CACHE_MAX`, the timeouts and `NO_CACHE` were read from the same map that `-e`, the repo config and the user variables write to: an app could use another app's cache or bypass its limits.
- Decision: those keys are taken only from the environment of the `acropolis` process; if they come through `-e`, `railpack.json` or the user variables they are discarded. `ACROPOLIS_CONFIG_FILE` must be relative and stay inside the app directory.

## D19. Hardening of what acropolis processes as root

- npm installation: lockfile paths are validated (`..`, absolute, NUL) and every directory that is created must resolve inside the installation tree (a repo symlink cannot redirect the write). Files are opened with `O_NOFOLLOW`. `http://` URLs and private or link-local hosts are rejected (`ACROPOLIS_ALLOW_PRIVATE_REGISTRY=1`). Extraction is streaming: a package is no longer decompressed whole in memory.
- Image layers: entries that go through a symlink of the layer itself are ignored; hardlinks only point to regular files inside the destination; `fchmod`/`fchown` on the descriptor.
- Layer assembly: paths in `deploy.inputs`, `deploy.paths` and build outputs must not contain `..`, and the root of each layer must resolve inside `src`, `work` or the app cache.
- Steps in an image: own PID namespace with a fresh `/proc` (previously the host's was mounted) and a mandatory read-only remount.
- Step output: lines of up to 64 KB, 32 MB of log per step, and the process group is killed as soon as the main process exits (a `sleep 600 &` hung the step). Command failures are a typed error: the class (user, or infra on SIGKILL) no longer depends on words in the user's output.

## D20. SPAs stay on Caddy

- `static-web-server` is 3.8 MB against 24.9 MB for `caddy:2-alpine`, but it lacks the `{path}.html` fallback the Caddyfile uses (Next with `output: 'export'` without `trailingSlash`, and Astro, generate `about.html`) and can't read `PORT` without a shell. Switching would break routes that work today to save ~20 MB on images that are already small. Discarded.
- Turbopack on Next 15 stays opt-in (`ACROPOLIS_NEXT_TURBOPACK=1`, only 15.5+ with a `next build` build): with `--turbopack` a `webpack()` function in the config is ignored with a warning, and plugins like `DefinePlugin` disappear without an error. Next 16 already uses Turbopack by default.

## D21. `patchedDependencies` (bun and pnpm)

- Found while measuring a real app: our installer ignored `patchedDependencies`, so the image shipped the unpatched package without any error. `bun install` and `pnpm install` (what Railpack runs) do apply the patch.
- Decision: after materializing `node_modules`, the patches from `package.json` (`patchedDependencies` and `pnpm.patchedDependencies`) and from `pnpm-workspace.yaml` are applied with our own diff applier (builders may have neither `patch` nor `git`) to every installed copy of the package and version. Patch contents go into the hash of the install step, so changing a patch invalidates the `node_modules` cache. If there are patches, the production dependency layer is assembled from an installed tree and not directly from the tarballs.

## D22. Security audit of 2026-10-08: what runs as root over a third-party repo

Model: the repo, its lockfiles, patches, configs and the `-e` variables are controlled by an attacker; the environment of the `acropolis` process belongs to the operator. Every finding has a test that fails without the fix.

- Host steps: own PID namespace with a fresh `/proc` (previously a `cat /proc/1/environ` in the build script read the registry credentials), read-only `/proc/{sys,sysrq-trigger,irq,bus,fs}` (`core_pattern` and `modprobe` allow escaping to the host without capabilities), recursively read-only `/sys` with `mount_setattr(AT_RECURSIVE)` (with the previous remount `/sys/fs/cgroup` stayed writable), `/dev` on a tmpfs with null, zero, full, random, urandom and tty (in a `--privileged` container the host disks were there), an allowlist of the capabilities that remain (CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, NET_BIND_SERVICE; previously `CAP_DAC_READ_SEARCH` remained, which allows `open_by_handle_at` outside the namespace), and `$DOCKER_CONFIG`, `$HOME/.docker` and `/root/.docker` covered by an empty tmpfs. If any of this fails, the step does not start. The same applies in the image rootfs, where in addition `lo` is brought up before dropping capabilities (previously it stayed down).
- Filesystem of host steps: everything is read-only except the build working directory, this app's cache, `/tmp` and `/var/tmp`. Previously a build script could write another app's cache in the same `ACROPOLIS_HOME`, `/usr/local/bin/acropolis` or `/etc/resolv.conf`; and since Rust and Go compile over the app directory without copying it, a `cargo fetch` without `Cargo.lock` wrote a `Cargo.lock` into the user's repo (that is how four Railpack examples and their plan snapshots got dirtied). Now a Rust app without `Cargo.lock` works on a copy. The image rootfs `resolv.conf` is also mounted read-only.
- Repo reads: config, `package.json`, lockfiles, `go.sum`, `Cargo.toml`, `rust-toolchain`, patches, version files, `Caddyfile` and `Staticfile` are only read if they resolve inside the app and are regular files. With `.nvmrc -> /proc/self/environ` the process environment showed up in the log, and with `Caddyfile -> /root/.docker/config.json` it ended up inside the image; json5, toml and yaml errors copy the offending line of the file.
- Writes as root: the `name` in `Cargo.lock`, module and version in `go.sum`, `packageManager`, toolchain versions and the native SPA `outDir` are validated before building paths (`name = "../../usr/lib/x86_64-linux"` deleted that host directory). Go module zips with absolute paths, tarball hardlinks with `..`, `.wh..` whiteouts, `.cargo-checksum.json` symlinks and patches that go through a symlink of the lockfile are rejected. The SPA bundler, which runs in-process, fails if a module, `@import`, `url()` or `public/` file resolves outside the app.
- Resources: PAX/GNU metadata up to 1 MiB, module zip up to 500 MiB uncompressed (Go's `modzip.MaxZipFile`), crate up to 512 MiB (cargo, CVE-2022-36114), in-memory responses up to 512 MiB, registry manifests, configs and tokens up to 16 MiB.
- Registry and network: credentials per target host (Docker Hub's were sent to the D12 mirror), `Authorization` only to the registry host (not to an upload `Location` on another host), token realm https only, digests only `sha256:<64 hex>` in references, descriptors and the on-disk image cache, references validated with the OCI distribution grammar, exact `Content-Range` in segmented downloads, `GOPROXY` with the same policy as npm registries (https and nothing private), and IPv4 inside IPv6 (`::ffff:`, NAT64) counts as private.
- Shared store: temporary files carry a random nonce and are created with `O_EXCL`; with `{pid}-{seq}` two containers (both with pid 1) wrote to the same inode and the blob ended up poisoned.
- Errors: the class of a command failure travels typed up to the exit code (previously a "JavaScript heap out of memory" in the output gave 75 and a SIGKILL gave 70); SIGTERM/SIGINT is 75; a panic exits with 70 and the `build_failed` event.
- Toolchain root of trust: Node (`SHASUMS256.txt`), Go (`.sha256`), Rust (channel manifest), uv, bun and pnpm are verified against a checksum from the same origin over HTTPS, without a signature. This protects against corruption, CDNs and mirrors, not against a compromise of nodejs.org, dl.google.com or static.rust-lang.org.
- Build steps (hardened host steps and image steps) run under a classic-BPF seccomp filter installed right before exec (`acropolis-exec/src/seccomp.rs`), modeled on Docker's default profile for a process without CAP_SYS_ADMIN: EPERM for keyctl/add_key/request_key, bpf, userfaultfd, perf_event_open, kexec_*, open_by_handle_at, module loading, every mount API (mount, umount2, pivot_root, move_mount, open_tree, fs*, mount_setattr), setns, unshare, ptrace, process_vm_readv/writev, swapon/swapoff, reboot, syslog, acct, settimeofday/clock_settime, iopl/ioperm. `clone` with any CLONE_NEW* flag gets EPERM; `clone3` gets ENOSYS so libcs fall back to `clone`, whose flags the filter can read. A foreign audit arch is killed and x32 syscalls get EPERM. Before, `unshare -U` worked inside a step and gave back every capability in a new user namespace. Cost: strace/gdb/ASan, bubblewrap, rootless podman/buildah and Chromium's namespace sandbox don't work *during the build* (runtime is unaffected).
- Image steps enter the rootfs with `pivot_root(".", ".")` plus a lazy unmount of the old root instead of `chroot`, so the host root is not in the step's mount namespace at all. The cleanup of bind targets in the upper dir never follows symlinks left by the step.
- Archive unpacking (image layers and app caches) stops at 32 GiB written per archive (hardlink copies included); npm package tarballs stop at 1 GiB unpacked (the largest real package, onnxruntime-node, is ~300 MB). Whole official images like `dotnet/sdk` or `ruby` unpack to 1–2 GB, so only compression bombs hit the caps.
- A public origin can't send Acropolis to a private host: the HTTP clients follow at most 10 redirects and refuse hops to private, loopback, link-local, CGNAT or ULA addresses (IPv4 embedded in IPv6 included) and https-to-http downgrades; registry token realms and blob redirects followed by hand get the same check (`acropolis_fetch::private_host`). Requests whose first host is already private (operator mirrors) are not restricted. DNS names that resolve to private IPs are still not filtered; the defense is the builder's network policy (README).
- Store durability without fsync: blobs are committed with write, `fsetxattr(user.acropolis.len)` and rename. After a machine crash, a blob whose rename survived but whose data didn't is caught by `Store::get` because its length doesn't match the journaled xattr, and is fetched again. Without user xattrs (or for older blobs) only 0-byte files are caught.
- Hedged segments (D2) now cancel the losing attempt as soon as a duplicate wins, which closes its connection.
- npm/pnpm/bun: pnpm dependency build scripts follow pnpm's own rules (`dangerouslyAllowAllBuilds`; `onlyBuiltDependencies`/`allowBuilds`; `neverBuiltDependencies` + `ignoredBuiltDependencies`; otherwise none with `packageManager: pnpm@10` or newer and all for older or unknown versions; lockfile 9.0 is written by pnpm 9, 10 and 11, so the version comes only from `packageManager`). `file:`/`link:` dependencies outside the app dir are installed as dangling symlinks, as `npm ci` does: the build context is the app dir and every read or write by Acropolis stays confined.
- yarn 2.x: `yarn.js` is checked against a built-in sha256 table covering every 2.x tag on repo.yarnpkg.com; unknown 2.x versions fail closed (`yarnPath` in `.yarnrc.yml` is the escape hatch).
- Non-UTF-8 file names and symlink targets in walked trees fail early with a config error (78) naming the file, because layers use UTF-8 tar paths; names excluded by the ignore rules are allowed.
- `acropolis gc` also deletes `layer-cache/*.json` records whose blob was evicted and `cache/images/*` entries older than 7 days.
- Provider precedence follows Railpack: php, go, java, rust, ruby, elixir, python, deno, dotnet, node, gleam, cpp, staticfile, shell. Before, any `package.json` made the app Node: a Cargo workspace with a bun workspace at the root (olimpia-cloud) or Django with Tailwind planned Node. `ACROPOLIS_PROVIDER` overrides.

## D23. Release profile without symbols

- Measured: the release binary was 194.8 MB with `debug = "line-tables-only"`; 39.5 MB without debuginfo and 31.3 MB with `strip = "symbols"`, the same as the `strip` the Dockerfile already did. Panic messages keep file and line; for backtraces, `CARGO_PROFILE_RELEASE_STRIP=none`. The `fast` profile is unchanged.
- Of `.text` (24.3 MB), the bundler (oxc, rolldown, lightningcss) takes ~37 % (D14), TLS with aws-lc 1.45 MB (before the switch to ring) and our own code 2.2 MB. `bench` and `e2e` are 238 KB: they stay in the binary, hidden from `--help`.
- rustls uses ring instead of aws-lc-rs (reqwest `rustls-no-provider`; `acropolis_fetch::ensure_tls()` installs the provider before any client is built). Measured on release `--bin acropolis` (thin LTO, cgu=16): 31.3 MB → 29.2 MB. Building the C crate alone, back to back on 5 vCPUs: aws-lc-sys 45 s, ring 14 s. Cargo.lock loses 16 packages. The cost: no post-quantum X25519MLKEM768 key exchange and no ECDSA P-521 certificates.
- `codegen-units = 1` in `[profile.release]`: 29.2 MB → 25.0 MB (−14 %); a cold release build goes from 4m39s to 7m19s on 5 vCPUs. `fast` keeps 256.

## D24. MSRV 1.96

- oxc 0.152 (via rolldown, D14) requires rustc 1.96; with `rust-version = "1.90"` and `rust:1.90-bookworm` the builder image did not compile with `--locked`. CI checks the MSRV declared in `Cargo.toml`.

## D25. Releases and CI

- release-plz in `git_only` mode (nothing is published to crates.io): a single `v{{version}}` tag, created by `acropolis-cli`; the libraries have no tag of their own, but their commits bump the version and go into `CHANGELOG.md` (with `release = false` they would be left out). PR titles follow Conventional Commits and PRs are squash-merged.
- Release binaries are built inside `rust:<version>-bookworm` (glibc 2.36, D9) for x86_64 and arm64, with `SHA256SUMS` and a provenance attestation. The builder image `ghcr.io/olimpiacloud/acropolis-builder` is multi-arch (linux/amd64 and linux/arm64), built natively on one runner per arch, pushed by digest and merged into one index with `imagetools create`; SBOM and provenance per platform, attestation on the index.
- CI runs the tests as root (`sudo -E` as the cargo runner) so the sandbox tests are not skipped, and compares the plan snapshots against the Railpack examples pinned to a commit, on a runner with glibc 2.39, because Rust plans depend on the host glibc (D9). The 131 plans use no network.

## D26. Next standalone: when the app's own `output` is honored

- With `output: 'standalone'` in the config, the image starts with `server.js` only if the start is `next start [-p|-H]` (and it honors `-p`); with a custom server (`node server.js`) that start is used, as Railpack does. Previously the generated `server.js` overwrote the user's and the port stayed at 3000.
- With `outputFileTracingRoot`, standalone is not forced and the app starts with `next start`: `server.js` ends up nested in a directory that cannot be predicted.

## D27. Rust: toolchain raised to what the locked crates need, and fewer builds inside the rust image

- Found building olimpia-cloud's API (Axum + sqlx 0.9) with no pinned toolchain: Railpack's default (1.89, which we keep for parity, and the e2e checks it) fails with `sqlx@0.9.0 requires rustc 1.94.0`. Railpack fails the same way; `docker build` with `rust:1` works because it takes the latest stable.
- Decision: when the version is not pinned (Railpack default or the edition minimum) and there is a `Cargo.lock`, the toolchain step waits for the vendored crates and raises the version to the highest `rust-version` among them (log line `the locked crates need rustc X`). Pinned versions (`rust-toolchain`, `rust-version`, `.rust-version`, `RUST_VERSION`, mise) are never changed. The Railpack examples keep their expected rustc.
- `Cargo.lock` lists optional dependencies whatever features are on, so every sqlx app locked `libsqlite3-sys` and was built inside the `rust` image with network (D9's fallback for system libraries), even with only `postgres`. Now the workspace manifests decide (dependency tables, `[workspace.dependencies]`, `target.*` tables, renamed packages and `[features]` entries like `sqlx/sqlite-unbundled`): the system libsqlite3 is needed only for sqlx with `sqlite-unbundled` (sqlx's `sqlite` bundles it) or rusqlite/libsqlite3-sys without a `bundled*` feature. The olimpia-cloud API now builds on the host.
- `zig cc` to pin the target glibc (pending in D9) is still not done: the builder image is Debian 12 like the runtime bases, so glibc matches there, and the check that fails a build on a newer host already prevents a broken image.
