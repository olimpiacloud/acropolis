# Builder image

`docker build -f deploy/builder/Dockerfile -t acropolis-builder .` from the repository root.

The image is Debian 12 (glibc 2.36, the same as the distroless and `bookworm-slim` runtime bases), so native addons compiled during a build run on the runtime image. It carries the host tools that install scripts expect (`python3`, `make`, `g++`, `pkg-config`, `git`) and toolchains prewarmed into `/var/lib/acropolis` (Node LTS, Go with its standard library precompiled, Bun, uv, Python), so a cold build only downloads the app's dependencies and the base image.

Run one build per container. The container needs `CAP_SYS_ADMIN` (mount and PID namespaces, overlayfs for Python, Ruby, PHP, Java, .NET and Elixir builds), so Acropolis runs as root inside it; the container is the boundary between customers. `tini` is PID 1, so `docker stop` reaches Acropolis and orphaned processes from build steps are reaped:

```
docker run --rm --privileged -v "$PWD:/app:ro" -v /tmp/out:/out acropolis-builder build /app --oci /out/image.tar --info /out/info.json
```

`--privileged` is not required: `--cap-add SYS_ADMIN --security-opt seccomp=unconfined --security-opt apparmor=unconfined` is enough (tested; Docker's default AppArmor profile denies `mount`). `$ACROPOLIS_HOME/work` must not be on overlayfs, because the kernel refuses an overlay upper dir there; the image declares it as a `VOLUME`, so a plain `docker run` already gets one. Other runtimes need a volume, `emptyDir` or tmpfs at `/var/lib/acropolis/work`; without it image steps fail with `is on overlayfs` (exit 75).

Operator settings come from the container environment (`ACROPOLIS_CACHE_KEY`, `ACROPOLIS_BUILD_TIMEOUT`, `ACROPOLIS_STEP_TIMEOUT`, `ACROPOLIS_BUILD_ID`, registry credentials); app settings come from `-e` and the app's `railpack.json` or `acropolis.json`.

Base images are pinned by digest; the comment at the top of the Dockerfile shows how to bump them. The Rust image must match `rust-version` in `Cargo.toml` (the minimum the locked dependencies accept). The build uses BuildKit cache mounts and `--locked`; the release profile already strips the binary.

## Using it as a PaaS builder (Olimpia)

One throwaway container (or VM) per build, the app's source extracted at `/src`. The contract a platform needs:

```
ACROPOLIS_BUILD_ID=<deploy id> ACROPOLIS_CACHE_KEY=<app id> ACROPOLIS_BUILD_TIMEOUT=1800 \
  acropolis --events json build "/src/$ROOT_DIR" -e KEY=VALUE... --oci /out/image.tar --info /out/info.json
```

- **Settings.** `ACROPOLIS_*` operator settings only count from the process environment; the customer's variables go through `-e`. They reach build steps, and the plan and `info.json` only carry `{env:NAME}` references, never the values; the image config doesn't get them (set runtime variables on the container that runs the image). Build steps run in their own PID namespace, so argv and the environment of `acropolis` (registry credentials, tokens) are not visible to them.
- **Output.** `--oci` writes an OCI image layout tar (`oci-layout`, `index.json`, `blobs/sha256/*`) that `regctl image import`, `skopeo` or `docker load` accept. Layers are zstd by default; pass `--compression gzip` if the hosts that run the image have a container runtime older than Docker 23 / containerd 1.5.
- **`info.json`.** Railpack-compatible keys (`detectedProviders`, `resolvedPackages`, `metadata`) plus `acropolisVersion`, `planHash`, `manifestDigest` (the image digest), `warnings`, `success`, `error`, `errorClass` and `exitCode`. It is written for failed builds too.
- **Logs.** `--events json` prints one JSON object per stderr line: `v` (schema version), `build_id`, `ts`, `t` (ms since start), `type` and the event's fields; a failed build ends with `type: "build_failed"` carrying `class`, `exit_code` and `error`. `human` prints readable lines.
- **Exit codes and fallback.** `0` built; `1` the app's own build failed, including `ACROPOLIS_BUILD_TIMEOUT`/`ACROPOLIS_STEP_TIMEOUT` (show the log, don't retry); `75` infrastructure (network, registry, disk, a step killed with SIGKILL, SIGTERM/SIGINT): retry once; `78` the app can't be built this way (unsupported stack or config, paths outside the app, private registry): fall back to another builder (Railpack/BuildKit) or show the error; `70` Acropolis bug: fall back and report it. Dockerfile builds stay on BuildKit.
- **Caches between builds.** The home (`/var/lib/acropolis`) dies with the container, but toolchains are prewarmed in this image. The per-app cache (Go and Cargo builds, `node_modules`, `.next/cache`, ...) travels as a file: `acropolis cache import <file>` before the build and `acropolis cache export <file>` after it, both with the same `ACROPOLIS_CACHE_KEY`. A missing or broken cache file is not an error for the build: skip the import.
- **Privileges.** Root with `CAP_SYS_ADMIN`, no seccomp or AppArmor profile that blocks `mount`/`unshare`, and `/var/lib/acropolis/work` on a filesystem that is not overlayfs (see above). Check a new runtime once with an image-step app, e.g. Railpack's `examples/python-uv`: on overlayfs it fails with `is on overlayfs` (exit 75).
- **Network.** Build steps with network can reach whatever the container reaches: block the cloud metadata endpoint and internal networks outside the container.
- **Dockerfile apps in the same image.** Extend this image with BuildKit and the platform's script tools, and override the entrypoint:

```dockerfile
FROM ghcr.io/olimpiacloud/acropolis-builder:<version>
COPY --from=moby/buildkit:<version> /usr/bin/buildkitd /usr/bin/buildctl /usr/bin/buildkit-runc /usr/bin/
RUN apt-get update && apt-get install -y --no-install-recommends curl jq pigz && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/build.sh"]
```
