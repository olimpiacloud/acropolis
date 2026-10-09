<!-- El título sigue Conventional Commits (feat:, fix:, docs:, refactor!: ...): queda como mensaje del commit y arma el changelog. -->

## Qué cambia y por qué

## Cómo se probó

<!-- Tests agregados, `scripts/plans.sh`, ejemplos de e2e corridos. -->

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` y `cargo test --workspace` pasan.
- [ ] Si cambia la detección, el diff de `tests/plans` está incluido y explicado.
- [ ] Si es una decisión de diseño con mediciones, está en `docs/decisions.md`.
