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

## D10. Política de versiones compatible con Railpack

- Los rangos se convierten a una versión "difusa" igual que Railpack: `>=20.0.0` → `20`, `^18.2` → `18`, `~22.1` → `22.1`, `20.x` → `20`, y se toma el último parche. Para Node eso permite elegir el tag `node:<v>-bookworm-slim` sin bajar `index.json`.
- Motivo: el ejemplo `node-oldest` (`engines: >=20.0.0`) espera Node 20; con "última versión que satisface" daba 26.

## D11. Arranque directo con entorno del gestor de paquetes

- Railpack arranca con `npm run start`/`pnpm run start`. Acropolis ejecuta el comando directo (un proceso menos, menos memoria, señales correctas) pero define `npm_config_user_agent`, `npm_lifecycle_event` y `npm_package_name`, y si el proyecto usa pnpm agrega el CLI (`pnpm` desde el registry de npm, verificado) en `/usr/local`.

## D12. Fallback de Docker Hub a un mirror verificado

- Durante las pruebas esta VM perdió conectividad con AWS us-east-1 (S3, STS y `registry-1.docker.io` colgaban en SYN; CloudFront y Cloudflare respondían). Si la conexión a Docker Hub (registry o `auth.docker.io`) falla, Acropolis cambia a `mirror.gcr.io`. Es seguro porque cada manifest y blob se verifica por digest. Timeout de conexión: 5 s.

## D13. Pasos fuera del host: rootfs de imagen con overlayfs

- Para lenguajes sin binarios reubicables (Ruby, Python con extensiones, PHP, Java, .NET, Elixir) y para paquetes apt, los pasos corren dentro del rootfs de la imagen oficial: capas desempaquetadas una vez como `lowerdir`, `upper` propio por paso, namespaces de mount/UTS/IPC (y de red si el paso es de build). El `upper` se convierte en capa OCI traduciendo whiteouts de overlay a whiteouts OCI.
- Costo: desempaquetar la imagen base (justo lo que se evita en Node/Go/Rust). Por eso solo se usa donde no hay alternativa.

## D14. Bundler propio para SPAs de Vite: instalación perezosa con índice por paquete

- Rolldown 1.2.12 como motor (crate de crates.io), con un plugin propio de `resolve_id`/`load`:
  - Un import "bare" busca el paquete en el plan del lockfile (resolución de Node por la cadena de `node_modules` del importador), baja ese tarball del store y lo indexa en memoria. No se extrae el paquete: se resuelven `exports` (condiciones `import`/`module`/`browser`/`production`/`default`, o `require`), `imports` (`#`), `browser`, `module`, `main` y extensiones contra el índice, sin syscalls, y solo se escribe a disco el archivo resuelto (más los `package.json` del camino, para `type`).
  - Al indexar un paquete se precargan sus `dependencies`/`peerDependencies` en segundo plano para cortar las "olas" secuenciales del grafo.
  - `sideEffects` del `package.json` se pasa a Rolldown para el tree-shaking.
  - CSS: Rolldown ya no empaqueta CSS. Los `.css` importados se recolectan en el orden de módulos de los chunks y se procesan con lightningcss (minify, `@import` inline, `url()` a `/assets/<nombre>-<hash>`).
  - Assets como Vite: archivo con hash bajo `/assets/` o data URL si pesa menos de 4 KB; imports absolutos contra `public/`.
  - Si el resolver propio no puede (campo `browser` como objeto, etc.) extrae el paquete completo y deja resolver a Rolldown.
- Se activa solo si `vite.config` no tiene más plugins que `@vitejs/plugin-react(-swc)`, no hay PostCSS/Tailwind y el script de build es `[tsc ... &&] vite build`. `ACROPOLIS_BUNDLER=vite` fuerza el camino de Vite.
- El type-check (`tsc -b`) se conserva: corre en paralelo con el bundle sobre la instalación completa; el build falla si falla.
- Medido (5 vCPU, frío):
  - Vite + React: el bundle baja 3 de 174 paquetes (1,4 MB) y tarda 0,43–0,49 s; build total 3,5–4,0 s contra 9,1 s con Vite.
  - React + MUI: 85 de 231 paquetes (7,1 MB) en 1,4–1,5 s; build total 7,1 s contra 15,6 s. Antes del índice por archivo el bundle tardaba 4,7 s: `@mui/icons-material` son 175 MB y ~49.000 archivos en disco para usar 2 íconos.
  - Equivalencia: mismo DOM (normalizando hashes y espacios entre tags), capturas idénticas pixel a pixel y sin errores de consola en Chromium headless (`tests/render/compare.mjs`).

## D15. Recursos de la VM durante las pruebas

- Compilar Rolldown en paralelo con el e2e disparó el OOM killer (7,7 GB). El e2e corre con 2 trabajos y cada build usa su propio directorio de rootfs que se borra al terminar el caso (sin eso el rootfs compartido llegó a 6,7 GB y llenó el disco).

## D16. Runtime de Node sobre distroless con shell de busybox

- Medido (registry, comprimido, amd64): `node:24-bookworm-slim` 80,8 MB; `gcr.io/distroless/nodejs24-debian12` 52,7 MB; la variante `:debug` 53,5 MB (agrega una capa de busybox de 740 KB, idéntica en todas las distroless `:debug`). `nodejs24-debian13` 55,3 MB, `nodejs26-debian13` 60,2 MB.
- Quedó: la base es `gcr.io/distroless/nodejs<major>-debian12:debug` (o `-debian13` si los scripts de producción compilaron contra una glibc más nueva), con una capa propia de tres symlinks: `/bin/sh → /busybox/sh`, `/usr/bin/env → /busybox/env` y `/usr/local/bin/node → /nodejs/bin/node`. Sin eso fallan los `#!/usr/bin/env node` de `node_modules/.bin` y `child_process.exec`.
- Solo si la versión pedida flota (`N`, `N.x`, `lts`, `latest`, `^N`, `>=N`): distroless publica la última versión de cada major (va un par de minors atrás), así que una versión exacta como `22.2.0` o `^24.15.0` sigue en `node:<v>-bookworm-slim`. Majors permitidos: 22, 24 y 26.
- Se cae a Debian slim si hay scripts de instalación en producción (salvo Next standalone y Nitro, que no instalan dependencias de producción aparte), start por shell o con npm/pnpm/yarn, puppeteer o playwright, paquetes apt de deploy, pasos custom, herramientas extra de mise o `ACROPOLIS_RUNTIME_BASE=debian`. La resolución se hace en tiempo de build: si el major no tiene imagen distroless se usa `node:<versión>-bookworm-slim` y la capa de symlinks queda vacía.
- Bun: `distroless/cc-debian12:debug` (9,9 MB) + capa de bun, sin ningún `node` (Railpack lo exige en `node-bun-no-deps`), solo cuando el start es `bun <archivo>` o un `bun run X` que termina en `bun <archivo>`. Un `bun run start` que llama a `node` sigue en Node.
- Riesgos revisados: Prisma necesita el shell (lo ejecuta para detectar OpenSSL) y distroless trae libssl3; sharp trae sus libs; no hay usuario `node` (uid 1000) ni `bash`/`npm` para `docker exec`.

## D17. Next sin `output` se construye como standalone

- `next start` necesita el `node_modules` de producción completo (en `node-next` la imagen pesaba 162,9 MB). Next 14, 15 y 16 leen `output: process.env.NEXT_PRIVATE_STANDALONE ? 'standalone' : undefined` como valor por defecto, así que basta con esa variable en el paso de build.
- La imagen lleva, además del standalone, los archivos de la app sin `node_modules`, `.next`, `.git` ni `public` (capa `layer-files`): `server.js` hace `chdir` a su directorio, así que siguen andando las lecturas relativas a `cwd` (contenido en markdown, configs de i18n).
- Solo se fuerza con Next ≥ 15 (o 13/14 con `sharp`), build exactamente `next build [--flags]`, start `next start` con `-p`/`-H` opcionales, sin `output`, `distDir`, `RuntimeConfig` ni `PHASE_` en la config, y sin dependencias que nft no traza (`dd-trace`, `newrelic`, `@opentelemetry/auto-instrumentations-node`, `@sentry/profiling-node`, `geoip-lite`, `pdfkit`, `@grpc/proto-loader`, `pino`, `next-i18next`). Yarn PnP y `bun.lockb` quedan afuera. `ACROPOLIS_NEXT_STANDALONE=0` lo apaga.
- Efecto colateral bueno: Next 15 con `next.config.ts` ya no necesita `typescript` en runtime, porque la config queda serializada en `server.js`. Para el camino `next start` se conserva `typescript` en las dependencias de producción en Next < 16.
- En monorepos el standalone queda anidado (`.next/standalone/<miembro>/server.js`): la imagen respeta esa estructura y el workdir es `/app/<miembro>`.

## D18. Perillas del operador separadas de las de la app

- Antes, `ACROPOLIS_CACHE_KEY`, `ACROPOLIS_CACHE_MAX`, los timeouts y `NO_CACHE` se leían del mismo mapa que escriben `-e`, la config del repo y las variables del usuario: una app podía usar la caché de otra o saltear sus límites.
- Quedó: esas claves solo se toman del entorno del proceso `acropolis`; si llegan por `-e`, por `railpack.json` o por las variables del usuario se descartan. `ACROPOLIS_CONFIG_FILE` tiene que ser relativo y quedar dentro del directorio de la app.

## D19. Endurecimiento de lo que acropolis procesa como root

- Instalación npm: las rutas de los lockfiles se validan (`..`, absolutas, NUL) y cada directorio que se crea tiene que resolver dentro del árbol de instalación (un symlink del repo no puede redirigir la escritura). Archivos con `O_NOFOLLOW`. URLs `http://` y hosts privados o link-local se rechazan (`ACROPOLIS_ALLOW_PRIVATE_REGISTRY=1`). La extracción es en streaming: un paquete ya no se descomprime entero en memoria.
- Capas de imagen: las entradas que pasan por un symlink de la propia capa se ignoran; los hardlinks solo apuntan a archivos regulares dentro del destino; `fchmod`/`fchown` sobre el descriptor.
- Armado de capas: rutas de `deploy.inputs`, `deploy.paths` y salidas de build sin `..`, y la raíz de cada capa tiene que resolver dentro de `src`, `work` o la caché de la app.
- Pasos en imagen: PID namespace propio con `/proc` nuevo (antes se montaba el del host) y remount de solo lectura obligatorio.
- Salida de los pasos: líneas de hasta 64 KB, 32 MB de log por paso, y el grupo de procesos se mata apenas termina el proceso principal (un `sleep 600 &` colgaba el paso). Los fallos de comandos son un error tipado: la clase (usuario o infra por SIGKILL) ya no depende de palabras en la salida del usuario.

## D20. SPAs siguen sobre Caddy

- `static-web-server` pesa 3,8 MB contra 24,9 MB de `caddy:2-alpine`, pero no tiene el fallback `{path}.html` que usa el Caddyfile (Next con `output: 'export'` sin `trailingSlash` y Astro generan `about.html`) ni lee `PORT` sin shell. Cambiarlo rompería rutas que hoy andan para ahorrar ~20 MB en imágenes que ya son chicas. Se descartó.
- Turbopack en Next 15 queda opt-in (`ACROPOLIS_NEXT_TURBOPACK=1`, solo 15.5+ con build `next build`): con `--turbopack` una función `webpack()` de la config se ignora con un warning y plugins como `DefinePlugin` desaparecen sin error. Next 16 ya usa Turbopack por defecto.

## D21. `patchedDependencies` (bun y pnpm)

- Encontrado al medir una app real: el instalador propio ignoraba `patchedDependencies`, así que la imagen salía con el paquete sin parchear y sin ningún error. `bun install` y `pnpm install` (lo que corre Railpack) sí aplican el parche.
- Quedó: después de materializar `node_modules` se aplican los parches de `package.json` (`patchedDependencies` y `pnpm.patchedDependencies`) y de `pnpm-workspace.yaml`, con un aplicador de diffs propio (los builders pueden no tener `patch` ni `git`), a cada copia instalada del paquete y versión. El contenido de los parches entra en el hash del paso de instalación, así que cambiar un parche invalida la caché de `node_modules`. Si hay parches, la capa de dependencias de producción se arma desde un árbol instalado y no directo desde los tarballs.

## D22. Auditoría de seguridad del 2026-10-08: lo que corre como root sobre un repo ajeno

Modelo: el repo, sus lockfiles, parches, configs y las variables `-e` los controla un atacante; el entorno del proceso `acropolis` es del operador. Cada hallazgo tiene un test que falla sin el arreglo.

- Pasos en el host: namespace de PID propio con `/proc` nuevo (antes un `cat /proc/1/environ` en el script de build leía las credenciales del registry), `/proc/{sys,sysrq-trigger,irq,bus,fs}` de solo lectura (`core_pattern` y `modprobe` permiten salir al host sin capabilities), `/sys` de solo lectura recursivo con `mount_setattr(AT_RECURSIVE)` (con el remount de antes `/sys/fs/cgroup` seguía escribible), `/dev` en tmpfs con null, zero, full, random, urandom y tty (en un contenedor `--privileged` estaban los discos del host), capabilities por lista de las que quedan (CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, NET_BIND_SERVICE; antes quedaba `CAP_DAC_READ_SEARCH`, que permite `open_by_handle_at` fuera del namespace) y `$DOCKER_CONFIG`, `$HOME/.docker` y `/root/.docker` tapados con un tmpfs vacío. Si algo de esto falla, el paso no arranca. Lo mismo en el rootfs de imagen, donde además `lo` se levanta antes de quitar las capabilities (antes quedaba apagado).
- Sistema de archivos de los pasos en el host: todo es de solo lectura salvo el directorio de trabajo del build, la caché de esta app, `/tmp` y `/var/tmp`. Antes un script de build podía escribir la caché de otra app en el mismo `ACROPOLIS_HOME`, `/usr/local/bin/acropolis` o `/etc/resolv.conf`; y como Rust y Go compilan sobre el directorio de la app sin copiarlo, un `cargo fetch` sin `Cargo.lock` lo escribía en el repo del usuario (así se ensuciaron cuatro ejemplos de Railpack y sus snapshots de planes). Ahora un Rust sin `Cargo.lock` trabaja sobre una copia. El `resolv.conf` del rootfs de imagen también se monta de solo lectura.
- Lecturas del repo: config, `package.json`, lockfiles, `go.sum`, `Cargo.toml`, `rust-toolchain`, parches, archivos de versión, `Caddyfile` y `Staticfile` solo se leen si resuelven dentro de la app y son archivos regulares. Con `.nvmrc -> /proc/self/environ` el entorno del proceso salía en el log, y con `Caddyfile -> /root/.docker/config.json` terminaba dentro de la imagen; los errores de json5, toml y yaml copian la línea del archivo.
- Escrituras como root: `name` de `Cargo.lock`, módulo y versión de `go.sum`, `packageManager`, versiones de toolchain y `outDir` del SPA nativo se validan antes de armar rutas (`name = "../../usr/lib/x86_64-linux"` borraba ese directorio del host). Zips de módulos Go con rutas absolutas, hardlinks de tarballs con `..`, whiteouts `.wh..`, symlinks `.cargo-checksum.json` y parches que pasan por un symlink del lockfile se rechazan. El bundler de SPAs, que corre dentro del proceso, falla si un módulo, `@import`, `url()` o archivo de `public/` resuelve fuera de la app.
- Recursos: metadatos PAX/GNU de hasta 1 MiB, zip de módulo de hasta 500 MiB descomprimido (`modzip.MaxZipFile` de Go), crate de hasta 512 MiB (cargo, CVE-2022-36114), respuestas en memoria de hasta 512 MiB, manifests, configs y tokens de registry de hasta 16 MiB.
- Registry y red: credenciales por host destino (las de Docker Hub viajaban al mirror de D12), `Authorization` solo al host del registry (no a un `Location` de upload en otro host), realm de token solo https, digests solo `sha256:<64 hex>` en referencias, descriptores y en la caché de imágenes en disco, referencias validadas con la gramática de OCI distribution, `Content-Range` exacto en las descargas segmentadas, `GOPROXY` con la misma política que los registries npm (https y nada privado) e IPv4 dentro de IPv6 (`::ffff:`, NAT64) cuenta como privada.
- Store compartido: los temporales llevan un nonce aleatorio y se crean con `O_EXCL`; con `{pid}-{seq}` dos contenedores (los dos con pid 1) escribían el mismo inodo y el blob quedaba envenenado.
- Errores: la clase del fallo de un comando viaja tipada hasta el código de salida (antes un "JavaScript heap out of memory" en la salida daba 75 y un SIGKILL daba 70); SIGTERM/SIGINT es 75; un panic sale con 70 y el evento `build_failed`.
- Raíz de confianza de los toolchains: Node (`SHASUMS256.txt`), Go (`.sha256`), Rust (manifiesto de canal), uv, bun y pnpm se verifican contra un checksum del mismo origen por HTTPS, sin firma. Protege contra corrupción, CDNs y mirrors, no contra un compromiso de nodejs.org, dl.google.com o static.rust-lang.org.
- Pendiente: seccomp (keyctl, bpf, userfaultfd, perf_event_open, `unshare(CLONE_NEWUSER)`), `pivot_root` en vez de `chroot`, tope de tamaño por entrada en `read_tarball` de npm, nombres DNS que resuelven a IPs privadas en lo que baja Acropolis (la defensa real es la red del builder, ver README).

## D23. Perfil release sin símbolos

- Medido: el binario release pesaba 194,8 MB con `debug = "line-tables-only"`; 39,5 MB sin debuginfo y 31,3 MB con `strip = "symbols"`, igual que el `strip` que ya hacía el Dockerfile. Los mensajes de panic conservan archivo y línea; para backtraces, `CARGO_PROFILE_RELEASE_STRIP=none`. El perfil `fast` no se toca.
- Del `.text` (24,3 MB), el bundler (oxc, rolldown, lightningcss) ocupa ~37 % (D14), TLS con aws-lc 1,45 MB y el código propio 2,2 MB. `bench` y `e2e` son 238 KB: quedan en el binario, ocultos de `--help`.
- Pendiente medir: rustls con ring en vez de aws-lc (~1,4 MB y sin compilar C) y `codegen-units = 1`.

## D24. MSRV 1.96

- oxc 0.152 (vía rolldown, D14) pide rustc 1.96; con `rust-version = "1.90"` y `rust:1.90-bookworm` la imagen de builder no compilaba con `--locked`. La CI chequea la MSRV que declara `Cargo.toml`.

## D25. Releases y CI

- release-plz en modo `git_only` (no se publica en crates.io): un solo tag `v{{version}}` lo crea `acropolis-cli`; las bibliotecas no tienen tag propio pero sus commits suben la versión y entran en `CHANGELOG.md` (con `release = false` quedarían afuera). Los títulos de PR siguen Conventional Commits y se integra con squash.
- Los binarios del release se compilan dentro de `rust:<versión>-bookworm` (glibc 2.36, D9) para x86_64 y arm64, con `SHA256SUMS` y atestación de procedencia; la imagen de builder va a `ghcr.io/olimpiacloud/acropolis-builder` con SBOM.
- La CI corre los tests como root (`sudo -E` como runner de cargo) para que los del sandbox no se salteen, y compara los snapshots de planes contra los ejemplos de Railpack fijados a un commit en un runner con glibc 2.39, porque los planes de Rust dependen de la glibc del host (D9). Los 131 planes no usan red.

## D26. Next standalone: cuándo se respeta el `output` propio

- Con `output: 'standalone'` en la config, la imagen arranca con `server.js` solo si el start es `next start [-p|-H]` (y respeta `-p`); con un servidor propio (`node server.js`) se usa ese start, como Railpack. Antes el `server.js` generado pisaba el del usuario y el puerto quedaba en 3000.
- Con `outputFileTracingRoot` no se fuerza standalone y se arranca con `next start`: `server.js` queda anidado en un directorio que no se puede predecir.
