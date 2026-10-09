# Cómo contribuir

Gracias por el interés. Los issues y los PR son bienvenidos en español o en inglés; el código, los mensajes de error y los commits van en inglés.

## Requisitos

- Linux x86_64 o arm64. Acropolis usa namespaces de mount, PID y red y overlayfs, así que no compila ni corre en macOS o Windows (sirve una VM o un contenedor `--privileged`).
- Rust estable reciente (la versión mínima es el `rust-version` de `Cargo.toml`), con `rustfmt` y `clippy`.
- Un compilador de C (`cc`) para las dependencias nativas (zstd, liblzma, aws-lc).
- Para builds completos: root. `acropolis plan` y la mayoría de los tests corren sin root; los pasos de build dentro de imágenes y los tests del sandbox necesitan root.
- Para la suite e2e: Docker.

## Compilar y probar

```
cargo build --bin acropolis           # target/debug/acropolis
cargo test --workspace                # tests unitarios
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test --workspace  # como root, como la CI: incluye los tests del sandbox
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

La CI corre lo mismo más `cargo deny check` (licencias, advisories y orígenes de las dependencias, ver `deny.toml`), la versión mínima de Rust y los snapshots de planes.

Los scripts de `scripts/` (`build.sh`, `test.sh`, `e2e.sh`) envuelven cargo en una unidad de systemd con límite de memoria para la VM de desarrollo del equipo; no hacen falta para contribuir.

## Snapshots de planes

`tests/plans` guarda la salida de `acropolis plan --json` para los 131 ejemplos de [Railpack](https://github.com/railwayapp/railpack). Es la forma más rápida de ver si un cambio altera la detección:

```
git clone https://github.com/railwayapp/railpack ../acropolis-ext/railpack
git -C ../acropolis-ext/railpack checkout <RAILPACK_REF de .github/workflows/ci.yml>
cargo build --bin acropolis
BIN=target/debug/acropolis scripts/plans.sh            # compara
BIN=target/debug/acropolis scripts/plans.sh update     # regenera, si el cambio es intencional
```

`ACROPOLIS_EXT` apunta a otro directorio si el clon no está junto al repo. Si un PR cambia planes, el diff de `tests/plans` tiene que estar en el PR y la descripción tiene que explicar por qué.

## e2e

`scripts/e2e.sh` construye cada ejemplo de Railpack, lo corre con `docker run` y chequea la salida (necesita root, Docker y `target/fast/acropolis`, que deja `scripts/build.sh` o `cargo build --profile fast`). Con `--filter <ejemplo>` corre uno solo. No es obligatorio para un PR, pero si tocás detección, instalación de dependencias o armado de capas, correr los ejemplos afectados ahorra una vuelta de revisión.

## Pull requests

- Un cambio por PR, con un test cuando se pueda: un test de comportamiento que falle sin el cambio.
- El título del PR sigue [Conventional Commits](https://www.conventionalcommits.org/es/v1.0.0/) (`feat(npm): ...`, `fix(oci): ...`, `docs: ...`, `refactor!: ...` para cambios incompatibles). Los PR se integran con squash y el título queda como mensaje del commit: de ahí salen la versión siguiente y `CHANGELOG.md`. Un check de la CI valida el título.
- Si el cambio es una decisión de diseño con mediciones (rendimiento, tamaño de imagen, seguridad), agregala a [`docs/decisions.md`](docs/decisions.md).
- No hace falta firmar los commits ni agregar `Signed-off-by`.

## Versiones

Las publica [release-plz](https://release-plz.dev): con cada merge a `main` abre o actualiza un PR de release con la versión nueva y el changelog; al mergearlo crea el tag `vX.Y.Z` y el release de GitHub, y la CI adjunta los binarios para Linux x86_64 y arm64 (glibc 2.36 o más nueva), sus checksums y la atestación de procedencia, y publica la imagen `ghcr.io/olimpiacloud/acropolis-builder`.

El workflow `release-plz` usa el token de una GitHub App (con un `GITHUB_TOKEN` el PR de release no correría la CI ni el release dispararía `release.yml`). Hasta que exista, el workflow se saltea. Para activarlo: crear la App en la organización (sin webhook, permisos Contents y Pull requests de lectura y escritura, instalada solo en este repo), guardar su client ID como variable del repo y la clave privada como secreto del environment `release`:

```
gh variable set RELEASE_PLZ_CLIENT_ID -R olimpiacloud/acropolis --body <client-id>
gh secret set RELEASE_PLZ_PRIVATE_KEY -R olimpiacloud/acropolis --env release < app-private-key.pem
```

La variable va a nivel repo y no del environment porque el `if` del job se evalúa antes de entrar al environment.

## Seguridad

Las vulnerabilidades no se reportan en issues: ver [SECURITY.md](SECURITY.md).

## Licencia

Al contribuir aceptás que tu aporte se publique bajo la misma licencia doble del proyecto, Apache-2.0 o MIT, a elección de quien lo use.
