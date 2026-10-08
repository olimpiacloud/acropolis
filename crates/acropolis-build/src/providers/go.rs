use crate::detect::{Env, GoApp};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::Result;
use std::collections::BTreeMap;

pub const STATIC_BASE: &str = "gcr.io/distroless/static-debian12";
pub const CGO_BASE: &str = "gcr.io/distroless/base-debian12";

pub fn plan(app: &GoApp, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "go");
    b.fact("go", format!("{} ({})", app.go.spec, app.go.source));
    b.fact("module", app.module.clone());
    b.fact("package", app.package.clone());
    b.fact("cgo", if app.cgo { "enabled" } else { "disabled" });
    let base = if app.cgo { CGO_BASE } else { STATIC_BASE };
    b.step(
        "base",
        format!("resolve {base}"),
        Action::ResolveBase { image: base.into() },
        &[],
    );
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "go",
        format!("go {}", app.go.spec),
        Action::Toolchain {
            tool: "go".into(),
            spec: app.go.spec.clone(),
            parts: vec![],
        },
        &[],
    );
    let vendored = app.dir.join("vendor").join("modules.txt").exists();
    let mut build_deps = vec!["go"];
    if app.has_sum && !vendored {
        let sum = std::fs::read(app.dir.join("go.sum"))?;
        b.step(
            "modules",
            "fetch Go modules",
            Action::GoModules {
                gosum_sha256: acropolis_store::sha256_bytes(&sum).hex(),
            },
            &[],
        );
        build_deps.push("modules");
    }
    let mut run_env = BTreeMap::new();
    run_env.insert("CGO_ENABLED".to_string(), if app.cgo { "1" } else { "0" }.to_string());
    for (k, v) in &env.vars {
        if !k.starts_with("ACROPOLIS_") && !k.starts_with("RAILPACK_") {
            run_env.insert(k.clone(), crate::env_ref(k, v));
        }
    }
    let mut argv = vec!["go".to_string(), "build".to_string()];
    if vendored {
        argv.push("-mod=vendor".to_string());
    }
    argv.extend([
        "-trimpath".to_string(),
        "-ldflags=-s -w".to_string(),
        "-o".to_string(),
        "{work}/out/app".to_string(),
    ]);
    argv.push(app.package.clone());
    b.step(
        "build",
        format!("go build {}", app.package),
        Action::Run {
            argv,
            env: run_env,
            network: false,
            cwd: "@app".into(),
        },
        &build_deps,
    );
    b.step(
        "layer-app",
        "layer app source",
        Action::Layer {
            dest: "app".into(),
            from: LayerFrom::AppSource { exclude: vec![] },
        },
        &[],
    );
    b.step(
        "layer-bin",
        "layer binary",
        Action::Layer {
            dest: "app/out".into(),
            from: LayerFrom::WorkFile {
                path: "{work}/out/app".into(),
                mode: 0o755,
            },
        },
        &["build"],
    );
    b.step(
        "push",
        "push image",
        Action::Push,
        &["base", "copy-base", "layer-app", "layer-bin"],
    );
    b.plan.image.layers = vec!["layer-app".into(), "layer-bin".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(vec!["/app/out".into()]);
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
