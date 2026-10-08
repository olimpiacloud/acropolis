# Builder image

`docker build -f deploy/builder/Dockerfile -t acropolis-builder .` from the repository root.

The image is Debian 12 (glibc 2.36, the same as the distroless and `bookworm-slim` runtime bases), so native addons compiled during a build run on the runtime image. It carries the host tools that install scripts expect (`python3`, `make`, `g++`, `pkg-config`, `git`) and toolchains prewarmed into `/var/lib/acropolis` (Node LTS, Go with its standard library precompiled, Bun, uv, Python), so a cold build only downloads the app's dependencies and the base image.

Run one build per container. The container needs `CAP_SYS_ADMIN` (mount and PID namespaces, overlayfs for Python, Ruby, PHP, Java, .NET and Elixir builds):

```
docker run --rm --privileged -v "$PWD:/app:ro" -v /tmp/out:/out acropolis-builder build /app --oci /out/image.tar --info /out/info.json
```

Operator settings come from the container environment (`ACROPOLIS_CACHE_KEY`, `ACROPOLIS_BUILD_TIMEOUT`, `ACROPOLIS_STEP_TIMEOUT`, `ACROPOLIS_BUILD_ID`, registry credentials); app settings come from `-e` and the app's `railpack.json` or `acropolis.json`.
