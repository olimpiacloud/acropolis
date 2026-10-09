# Changelog

Todos los cambios relevantes de Acropolis. El formato sigue [Keep a Changelog](https://keepachangelog.com/es-ES/1.1.0/) y el proyecto usa [versionado semántico](https://semver.org/lang/es/). Desde 0.1.0 las entradas las genera release-plz a partir de los títulos de los PR ([Conventional Commits](https://www.conventionalcommits.org/es/v1.0.0/)).

## [Unreleased]

## [0.1.0](https://github.com/olimpiacloud/acropolis/releases/tag/v0.1.0) - 2026-10-08

Primera versión pública.

### Construcción de imágenes

- Un único binario `acropolis` con `build`, `plan`, `prewarm` y `cache export|import`. Salida a un registry (`-t`), a un tar OCI cargable con `docker load` (`--oci`) y un resumen JSON (`--info`); eventos JSON por línea con `--events json`.
- Store direccionado por contenido, descargas verificadas por checksum, segmentadas y con duplicación del segmento rezagado; la base se copia de registry a registry por digest y las capas nuevas se comprimen en paralelo.
- Pasos de build aislados en namespaces de mount, PID y red; pasos dentro del rootfs de imágenes oficiales con overlayfs (Python, Ruby, PHP, Java, .NET, Elixir).
- Cachés por app que sobreviven a builders efímeros (`acropolis cache export|import`).

### Ecosistemas

- Node: instalador propio para npm, pnpm, yarn 1 y bun (yarn berry y `bun.lockb` con sus CLIs oficiales), hoisting, scripts de instalación, `patchedDependencies` de bun y pnpm, filtrado de binarios por plataforma en yarn; Next (standalone forzado, Turbopack), Nuxt, Astro, SvelteKit, Remix, React Router, TanStack Start, Angular, Vite (bundler nativo con Rolldown), Nx y Turborepo.
- Go (módulos verificados contra `go.sum`, `go.work`, apps sin `go.mod`), Rust (toolchain desde el manifiesto de canal y crates vendorizados), Python (uv, poetry, pdm, pipenv, pip), Ruby/Rails, PHP/Laravel, Java, .NET, Elixir, Gleam, Deno, C/C++, sitios estáticos y scripts.
- Runtimes chicos: distroless para Node, Bun, Go y Rust; imágenes oficiales slim para el resto.
- Compatible con `railpack.json` y `acropolis.json`.

### Producción

- Ajustes del operador (`ACROPOLIS_CACHE_KEY`, `ACROPOLIS_CACHE_MAX`, `ACROPOLIS_BUILD_TIMEOUT`, `ACROPOLIS_STEP_TIMEOUT`, `ACROPOLIS_BUILD_ID`) separados de los de la app.
- Endurecimiento de lo que se procesa como root: rutas de lockfiles, capas y config validadas, extracción en streaming, límites de logs y de cuerpos HTTP, timeout de build por defecto.
- Códigos de salida tipados (0, 1, 70, 75, 78) e imagen de builder Debian 12 en `deploy/builder`.

### Verificación

- 154 de 156 casos de los ejemplos de Railpack, 8/8 de la suite Next.js y 37 de 73 casos de Nixpacks; snapshots de `acropolis plan --json` en `tests/plans`.
