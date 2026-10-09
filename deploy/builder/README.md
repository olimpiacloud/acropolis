# Builder image

`docker build -f deploy/builder/Dockerfile -t acropolis-builder .` from the repository root.

The image is Debian 12 (glibc 2.36, the same as the distroless and `bookworm-slim` runtime bases), so native addons compiled during a build run on the runtime image. It carries the host tools that install scripts expect (`python3`, `make`, `g++`, `pkg-config`, `git`) and toolchains prewarmed into `/var/lib/acropolis` (Node LTS, Go with its standard library precompiled, Bun, uv, Python), so a cold build only downloads the app's dependencies and the base image.

Run one build per container. The container needs `CAP_SYS_ADMIN` (mount and PID namespaces, overlayfs for Python, Ruby, PHP, Java, .NET and Elixir builds), so Acropolis runs as root inside it; the container is the boundary between customers. `tini` is PID 1, so `docker stop` reaches Acropolis and orphaned processes from build steps are reaped:

```
docker run --rm --privileged -v "$PWD:/app:ro" -v /tmp/out:/out acropolis-builder build /app --oci /out/image.tar --info /out/info.json
```

Operator settings come from the container environment (`ACROPOLIS_CACHE_KEY`, `ACROPOLIS_BUILD_TIMEOUT`, `ACROPOLIS_STEP_TIMEOUT`, `ACROPOLIS_BUILD_ID`, registry credentials); app settings come from `-e` and the app's `railpack.json` or `acropolis.json`.

Base images are pinned by digest; the comment at the top of the Dockerfile shows how to bump them. The Rust image must match `rust-version` in `Cargo.toml` (the minimum the locked dependencies accept). The build uses BuildKit cache mounts and `--locked`; the release profile already strips the binary.
