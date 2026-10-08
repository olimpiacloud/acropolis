# Acropolis

Acropolis toma el código de una app y devuelve una imagen OCI lista para correr, sin Dockerfile y sin daemon. Detecta el lenguaje y el framework, baja toolchains y dependencias verificadas, compila y arma la imagen. Es un único binario en Rust (`acropolis`) y es el builder de [Olimpia](https://olimpia.dev).

```
acropolis build ./mi-app -t registry.example.com/equipo/mi-app:latest
acropolis build ./mi-app --oci imagen.tar --info info.json
acropolis plan ./mi-app
```

Lo escribimos porque en un PaaS cada build arranca en una máquina vacía, y ahí las herramientas que había eran lentas, pesadas o hacían imágenes enormes.

## Cómo nos aseguramos de que anda

No confiamos en nuestras propias apps de prueba: usamos los tests de otros builders tal como los publican. Cada caso construye la imagen con Acropolis, la corre con `docker run` y chequea la salida o un pedido HTTP, con los mismos tiempos que el original.

| suite | qué es | resultado |
|---|---|---|
| Ejemplos de [Railpack](https://github.com/railwayapp/railpack/tree/main/examples) | 131 apps y 156 casos: Node (npm, pnpm, yarn, bun, Next, Nuxt, Astro, SvelteKit, Remix, Angular, Nx, Turborepo), Python (pip, uv, poetry, pdm), Go, Rust, Ruby/Rails, PHP/Laravel, Java, .NET, Elixir, Gleam, Deno, sitios estáticos y scripts | **154 pasan**; los 2 restantes son para arm64 y se saltean en x86 |
| Next.js y Turbopack (`tests/suites/next`) | Next 15 con `--turbopack`, Next 16 con Turbopack por defecto, `--webpack`, `output: standalone`, `output: export`, `next/image`, lecturas de archivos en runtime y un monorepo Turborepo; con npm, pnpm, yarn y bun | **8/8** |
| Tests de [Nixpacks](https://github.com/railwayapp/nixpacks) (`scripts/nixpacks-suite.py`) | 73 casos convertidos de `tests/docker_run_tests.rs` | **37 pasan**; el resto son lenguajes que tampoco soporta Railpack (Clojure, Crystal, Dart, Haskell, Scala, Scheme, Swift, Zig), configs propias de Nixpacks (`nixpacks.toml`, `NIXPACKS_*`) o apps que esperan el compilador dentro de la imagen final |
| Snapshots de planes (`tests/plans`) | `acropolis plan --json` de los 131 ejemplos de Railpack | cualquier cambio de detección aparece como diff antes de construir nada |
| Tests unitarios | instalación de paquetes, capas, sandbox, errores | 62 |

Esas suites encontraron bugs que nuestras apps nunca habrían mostrado: yarn v1 instalando binarios de todas las plataformas (177 MB de `sharp` para darwin, windows y arm en una imagen linux), los `node_modules` de los miembros de un workspace pnpm que no llegaban a la imagen, Next 15 que necesitaba `typescript` en runtime para leer `next.config.ts`, parches de `patchedDependencies` que no se aplicaban, hardlinks duplicados que inflaban las capas, Go sin `go.mod` y Java con un Gradle viejo.

Las imágenes se prueban con `docker pull` desde un registry local; con `E2E_OCI=1` el harness además carga el `--oci` tar con `docker load`, que es como las consume Olimpia.

## Qué vimos que nadie resolvía

- **El build en frío es lento.** En nuestro benchmark Railpack tarda de 54 a 170 s porque primero baja su imagen de builder y BuildKit no reintenta una descarga que se queda colgada: vimos una capa de 68 MB bajar a 200 KB/s durante 10 minutos. Docker es más rápido, pero solo si alguien escribió un Dockerfile multistage a mano.
- **Las imágenes vienen infladas.** Railpack deja en la imagen las devDependencies, la caché y su gestor de toolchains: una app Next de ejemplo pesa 345 MB contra 105 MB con un Dockerfile bien hecho.
- **Se usa mucha memoria para poco.** Railpack pasa los 2,9 GB de RAM para construir un servidor Express con 7,5 MB de dependencias.
- **Los rebuilds no aprovechan nada.** Con Docker, cambiar una línea de una app Go recompila todo (41 s).
- **En un builder efímero no sobrevive ninguna caché.**

## Qué resultó

Benchmark en frío y en rebuild con 2 CPUs, las tres herramientas el mismo día y en las mismas condiciones, mediana de 2 corridas. Docker usa un Dockerfile multistage idiomático para cada app (`bench/dockerfiles`).

| app | tiempo en frío (Docker / Railpack / **Acropolis**) | imagen (Docker / Railpack / **Acropolis**) | RAM pico (Docker / Railpack / **Acropolis**) |
|---|---|---|---|
| Express | 16,1 s / 53,7 s / **6,6 s** | 81,2 / 147,5 / **54,6 MB** | 562 / 2955 / **83 MB** |
| Go | 77,9 s / 85,3 s / **41,7 s** | 4,3 / 40,8 / **4,2 MB** | 2221 / 2944 / **1088 MB** |
| Rust | 75,3 s / 112,4 s / **48,2 s** | 10,0 / 38,0 / **11,4 MB** | 2910 / 4523 / **1509 MB** |
| Vite + React | 23,0 s / 55,3 s / **5,2 s** | 26,4 / 54,7 / **25,0 MB** | 933 / 2863 / **594 MB** |
| Vite + MUI | 48,3 s / 77,8 s / **8,4 s** | 26,5 / 54,7 / **25,0 MB** | 1329 / 3262 / **898 MB** |
| TanStack Start | 26,8 s / 78,1 s / **8,1 s** | 80,1 / 203,0 / **53,6 MB** | 1242 / 3575 / **960 MB** |
| Next 15 | 118,1 s / 170,5 s / **47,5 s** | 105,4 / 345,6 / **69,8 MB** | 3075 / 4560 / **1936 MB** |

En rebuild, después de cambiar un archivo: Go 41 s / 5,8 s / **1,1 s**, Rust 49 s / 15 s / **6,5 s** y Next 71 s / 81 s / **24,8 s**.

La RAM pico incluye page cache. La memoria anónima, que no se puede liberar, queda pareja con Docker en frío y por debajo en rebuild: el grueso es el compilador de cada lenguaje, no Acropolis.

Lo que más pesó:

- **Nada de imagen de builder.** Node, Go y Rust se bajan de sus fuentes oficiales con checksum y corren directo; las dependencias (npm, pnpm, yarn, bun, módulos de Go, crates) las instala Acropolis desde los lockfiles, verificadas mientras bajan.
- **Descargas que no se cuelgan.** Los blobs grandes se bajan en segmentos en paralelo, y si uno se atrasa se lanza un duplicado y gana el primero.
- **La base nunca se descomprime.** Las capas de la imagen base se copian de registry a registry por digest; solo se comprimen las capas nuevas, en paralelo.
- **Runtimes chicos.** Node y Bun corren sobre distroless con un shell mínimo cuando no hace falta nada de Debian, y las apps Next 15+ sin `output` configurado se construyen como standalone.
- **Cachés que se pueden llevar.** Go, Cargo, `.next/cache` y demás quedan en una caché por app que se exporta e importa como un `tar.zst` (`acropolis cache export|import`), para guardarla entre builders efímeros.

Las mediciones y lo que probamos y descartamos están en [`docs/decisions.md`](docs/decisions.md).

## Qué soporta

| ecosistema | runtime de la imagen |
|---|---|
| Node: npm, pnpm, yarn 1, yarn berry, bun; Next, Nuxt, Astro, SvelteKit, Remix, React Router, TanStack Start, Angular, Vite, Nx, Turborepo | distroless con shell mínimo cuando se puede; si no, `node:<versión>-bookworm-slim`. Next standalone, Nitro, SPA sobre Caddy |
| Bun | `distroless/cc` + bun, o Debian slim |
| Go | `distroless/static` |
| Rust | `distroless/cc` |
| Python: uv, poetry, pdm, pipenv, pip | `python:<versión>-slim` |
| Ruby, PHP/Laravel, Java, .NET, Elixir, Gleam, Deno, C/C++ | la imagen oficial slim de cada uno |
| Sitios estáticos y scripts | Caddy, Debian slim |

Respeta `railpack.json` y `acropolis.json`: pasos propios, paquetes extra, `buildAptPackages`, `deploy.aptPackages`, `deploy.paths` y `deploy.inputs`.

## En producción

- **Un build por contenedor.** Acropolis corre como root porque usa namespaces de mount y PID y overlayfs. Los pasos de build corren aislados: sin acceso de escritura al store ni a los toolchains, sin capabilities peligrosas y con su propio namespace de red si no necesitan red. Lo que Acropolis procesa como root (lockfiles, capas de imágenes, rutas de la config) se valida para que no pueda escribir ni leer fuera de su árbol. Aun así no es una frontera de VM: para builds de distintos clientes, un contenedor desechable por build.
- **Ajustes del operador.** `ACROPOLIS_CACHE_KEY`, `ACROPOLIS_CACHE_MAX`, `ACROPOLIS_BUILD_TIMEOUT` (1 h por defecto), `ACROPOLIS_STEP_TIMEOUT` y `ACROPOLIS_BUILD_ID` se leen solo del entorno del proceso; si llegan por `-e` o por la config del repo se ignoran.
- **Salida.** `-t` pushea a un registry, `--oci` escribe un tar cargable con `docker load` y `--info` deja un resumen en JSON. Con `--events json` cada línea de stderr es un evento con versión de esquema y `build_id`.
- **Códigos de salida.** 0 ok, 1 falló el build de la app, 70 bug de Acropolis, 75 problema de infraestructura (se puede reintentar), 78 la app no se puede construir así como está configurada.
- **Imagen de builder.** [`deploy/builder`](deploy/builder) tiene una imagen Debian 12 con las herramientas de compilación nativa y los toolchains precalentados.

## Desarrollo

```
scripts/build.sh               # compila target/fast/acropolis
scripts/test.sh --workspace    # tests unitarios
scripts/plans.sh               # compara los planes de los ejemplos de Railpack
scripts/e2e.sh                 # corre los ejemplos de Railpack (ACROPOLIS_EXT apunta al clon)
scripts/bench.sh               # benchmark contra Docker y Railpack
```

Los scripts esperan, junto al repo, un directorio `acropolis-ext` con el clon de Railpack y su binario (o la ruta en `ACROPOLIS_EXT`).

## Licencia

Apache-2.0 o MIT.
