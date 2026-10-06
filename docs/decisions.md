# Registro de decisiones

Cada entrada: qué se probó, qué número dio, qué quedó y qué se descartó. Las mediciones son de la VM de desarrollo (5 vCPU AMD EPYC-Rome, 7,7 GB, Ubuntu 24.04, glibc 2.39) salvo que se indique otra cosa.

## D1. Runtime de Node para servidores sin build: copiar `node:<versión>-bookworm-slim` por digest

- Medido: el tarball oficial de Node trae 67 MB de `include/` que solo sirven para node-gyp. Bajar + descomprimir `.tar.xz` cuesta 3,9 s de pared y 3,4 s de CPU; `.tar.gz` 2,1 s y 1,7 s.
- Recomprimir solo el binario `node` (118 MB) como capa: gzip-6 7,3 s, zstd-3 0,9 s (0,37 s con 4 hilos), zstd-1 0,64 s.
- Copiar la imagen oficial registry a registry no gasta CPU (0,8 CPU-s el build completo de Express).
- Quedó: para apps Node sin paso de build, la base es la imagen oficial por tag de versión (`22` → `node:22-bookworm-slim`, exacta → `node:23.5.0-bookworm-slim`). Rangos (`>=18`) se resuelven contra `nodejs.org/dist/index.json`.
- Pendiente: medir la alternativa `distroless/cc` + capa de `node` construida desde el toolchain ya bajado para apps con build (SSR), donde el toolchain se baja igual.

## D2. Descargas segmentadas por rangos con duplicación del segmento rezagado

- Medido: los CDNs de registries rinden 5–14 MB/s por stream desde esta VM; nodejs.org ~34 MB/s; npm 60–74 MB/s. Un `curl` a nodejs.org quedó colgado 10 minutos sin bytes; dl.google.com se estancó al 96%. En una corrida de Railpack, una capa de 68 MB de su imagen builder bajó a ~200 KB/s durante 10 minutos (BuildKit no reintenta streams lentos).
- Quedó: blobs ≥ 12 MB se bajan en segmentos de 8 MB, 6 en paralelo sobre conexiones HTTP/1.1 independientes (con HTTP/2 los duplicados viajarían por la misma conexión lenta). Si el segmento que bloquea la salida en orden tarda más de max(2,5 s, 2,5 × mediana), se lanza un duplicado y gana el primero. Streams sin bytes por 6 s se cortan y se reanudan con `Range`.
- Resultado: copiar `node:22-bookworm-slim` (80 MB) de Docker Hub al registry destino pasó de 12,2 s a 2,9 s.

## D3. Arrancar la copia de la base apenas llega el manifest de la plataforma

- La resolución de una base son tres pedidos en serie (índice, manifest, config). En mirror.gcr.io cada uno tarda ~1,2 s de servidor aun por digest; en Docker Hub ~0,3–0,5 s.
- Quedó: la copia de capas arranca después del manifest y el config se baja en paralelo. Además se pide el token de Docker Hub y ghcr.io sin esperar el 401.
- Express en frío: 6,0 s → 3,2–4,1 s con Docker Hub directo.

## D4. Las capas se escriben como fragmentos comprimidos en paralelo

- Una capa es una concatenación de miembros gzip (o frames zstd) independientes de ~1 MB, comprimidos en paralelo con rayon. Es gzip/zstd válido (multi-member) y determinístico; `diff_id` se calcula sobre el tar concatenado.
- `node_modules` de producción se escribe directo desde los tarballs del store, sin desempaquetar a disco: Express (76 paquetes, 7,5 MB sin comprimir) en 0,05 s.

## D5. Pasos de build sin red por namespace

- `unshare(CLONE_NEWNET)` (o `CLONE_NEWUSER|CLONE_NEWNET` sin root) con loopback levantado. Si el host no puede crear namespaces, el build falla salvo `--hermetic=off`.
- Excepción explícita y visible en el plan: `next build` corre con red (Next baja Google Fonts en build). El plan lo marca `build+net` y emite una advertencia.

## D6. Go: módulos verificados contra `go.sum` y module cache armado por Acropolis

- Se bajan los `.zip` y `.mod` del proxy en paralelo, se verifica el hash `h1:` (dirhash) contra `go.sum` y se escribe el layout del module cache (`<mod>@<v>/` + `.ziphash`). `go build` corre con `GOPROXY=off`, `GOFLAGS=-mod=readonly`, sin red.
- Del tarball de Go se omiten `src/cmd`, `test`, `api`, `doc`, `misc`, `*_test.go` y `testdata`: ~120 MB menos escritos de 243 MB.
- Medido (5 vCPU): toolchain 2,4 s, 34 módulos 2,8 s, `go build` 21,7 s. El compilador domina, como anticipaba el documento.

## D7. DNS dentro de BuildKit en el benchmark

- El `resolv.conf` del host apunta al stub de systemd-resolved (127.0.0.53). Los `RUN` de BuildKit en red bridge no lo alcanzan: `npm ci` colgó 353 s y mise falló resolviendo DNS.
- Neutralización: `bench/buildkitd.toml` fija `[dns] nameservers` a los mismos upstream que usa el host. Acropolis usa el resolver del host. No es una diferencia de producto.

## D8. Docker Hub directo en el benchmark, sin mirror

- mirror.gcr.io agrega ~1,2 s por pedido; Docker Hub directo es más rápido en latencia. El límite anónimo (100 pulls/h) alcanza para 5 apps × 3 herramientas × 2 corridas.
- Railpack baja sus imágenes de ghcr.io, que no se ve afectado.

## D9. Rust: toolchain desde el manifiesto de canal, crates vendorizados y base según glibc del host

- Componentes `rustc`, `cargo`, `rust-std` desde `static.rust-lang.org` verificados con el sha256 del manifiesto de canal, en paralelo y en streaming. `rustc` en `.tar.xz` pesa 84 MB contra 141 MB en `.tar.gz`; decodificar xz cuesta ~7,9 CPU-s, pero la CPU está ociosa mientras se baja el toolchain, así que se prefieren menos bytes.
- Crates desde `static.crates.io` verificados con el `checksum` de `Cargo.lock`, vendorizados con `.cargo-checksum.json` y `source.crates-io` reemplazado. `cargo build --locked --offline` sin red, con `CARGO_TARGET_DIR` fuera del repo.
- El binario se linkea contra la glibc del host: la base de runtime se elige con glibc ≥ la del host (`distroless/cc-debian12` hasta 2.36, `cc-debian13` hasta 2.41); si el host es más nuevo, el build falla explícitamente.
- Pendiente: usar `zig cc` como linker para fijar la versión de glibc objetivo y no depender de un `cc` en el host.
