# Security policy

## How to report a vulnerability

Don't open a public issue. Use GitHub private reporting: **Security** tab → **Report a vulnerability** ([direct link](https://github.com/olimpiacloud/acropolis/security/advisories/new)). The report is not public: only the maintainers and the reporter can see it.

Include, if you can:

- version (`acropolis --version`) and how you run it (container, privileges, `ACROPOLIS_*` variables);
- a minimal repo or lockfile that reproduces it, or the steps;
- what an attacker gains (reading or writing outside the app tree, escaping a step's sandbox, poisoning another app's cache, forging a verified download, etc.).

We acknowledge receipt in the advisory thread, coordinate the disclosure date with the reporter, and credit them in the advisory unless they prefer otherwise.

## Supported versions

Acropolis is at 0.x: only the latest published version receives security fixes. There are no backports to earlier versions.

| version | supported |
|---|---|
| latest `0.x` | yes |
| earlier | no |

## Threat model

Acropolis is designed to build third-party repos inside a PaaS (see the "In production" section of the [README](README.md) and decisions D18, D19 and D22 in [`docs/decisions.md`](docs/decisions.md)):

- **Untrusted:** all content of the app repo (code, lockfiles, `railpack.json`/`acropolis.json`, patches, build scripts) and the app variables (`-e`).
- **Trusted:** whoever operates the builder and the environment of the `acropolis` process (operator `ACROPOLIS_*` variables, registry credentials).
- **Semi-trusted:** registries, CDNs and mirrors; everything downloaded is verified by digest or checksum.

Within that model we are especially interested in: writes or reads outside the app tree or the app cache while processing lockfiles, layers or config paths; escapes from the build step sandbox to the store, the toolchains or the host; bypassing checksum verification; one app using or poisoning another app's cache; an app's config changing operator settings.

Out of scope: what a build script can do *inside* its own step (it is the customer's code and runs as such), and isolating builds from different customers that share a container. Acropolis is not a VM boundary: run one build per throwaway container.
