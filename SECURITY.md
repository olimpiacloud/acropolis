# Política de seguridad

## Cómo reportar una vulnerabilidad

No abras un issue público. Usá el reporte privado de GitHub: pestaña **Security** → **Report a vulnerability** ([enlace directo](https://github.com/olimpiacloud/acropolis/security/advisories/new)). El reporte no es público: lo ven los mantenedores y quien lo envía.

Incluí, si podés:

- versión (`acropolis --version`) y cómo lo corrés (contenedor, privilegios, variables `ACROPOLIS_*`);
- un repo o lockfile mínimo que lo reproduzca, o los pasos;
- qué consigue un atacante (leer o escribir fuera del árbol de la app, escapar del sandbox de un paso, envenenar la caché de otra app, falsificar una descarga verificada, etc.).

Confirmamos la recepción por el mismo hilo del advisory, coordinamos la fecha de publicación con quien reporta y le damos crédito en el advisory, salvo que prefiera lo contrario.

## Versiones con soporte

Acropolis está en 0.x: solo la última versión publicada recibe arreglos de seguridad. No hay backports a versiones anteriores.

| versión | soporte |
|---|---|
| última `0.x` | sí |
| anteriores | no |

## Modelo de amenazas

Acropolis está pensado para construir repos de terceros dentro de un PaaS (ver la sección "En producción" del [README](README.md) y las decisiones D18, D19 y D22 de [`docs/decisions.md`](docs/decisions.md)):

- **No confiable:** todo el contenido del repo de la app (código, lockfiles, `railpack.json`/`acropolis.json`, parches, scripts de build) y las variables de la app (`-e`).
- **Confiable:** quien opera el builder y el entorno del proceso `acropolis` (variables `ACROPOLIS_*` del operador, credenciales de registry).
- **Semiconfiable:** registries, CDNs y mirrors; todo lo que se baja se verifica por digest o checksum.

Dentro de ese modelo nos interesan especialmente: escrituras o lecturas fuera del árbol de la app o de la caché de la app al procesar lockfiles, capas o rutas de la config; escapes del sandbox de los pasos de build hacia el store, los toolchains o el host; saltear la verificación de checksums; que una app use o envenene la caché de otra; que la config de una app cambie ajustes del operador.

Fuera de alcance: lo que un script de build puede hacer *dentro* de su propio paso (es código del cliente y corre como tal), y aislar builds de clientes distintos que comparten contenedor. Acropolis no es una frontera de VM: corré un build por contenedor desechable.
