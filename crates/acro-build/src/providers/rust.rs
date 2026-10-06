use crate::detect::{Env, VersionSpec};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct RustApp {
    pub rust: VersionSpec,
    pub project: acro_cargo::RustProject,
    pub bin: String,
    pub package: Option<String>,
    pub has_lock: bool,
}

pub fn detect(dir: &Path, env: &Env) -> Result<RustApp> {
    let project = acro_cargo::read_project(dir)?;
    let rust = if let Some((v, k)) = env.config("RUST_VERSION") {
        VersionSpec { spec: v, source: k }
    } else if let Some(v) = crate::detect::tool_version(dir, "rust") {
        v
    } else if let Some(v) = ["rust-version.txt", ".rust-version"]
        .iter()
        .find_map(|f| std::fs::read_to_string(dir.join(f)).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()))
    {
        VersionSpec { spec: v, source: ".rust-version".into() }
    } else if let Some(c) = acro_cargo::toolchain_file(dir) {
        VersionSpec { spec: c, source: "rust-toolchain".into() }
    } else if let Some(v) = project.rust_version.clone() {
        VersionSpec { spec: v, source: "Cargo.toml rust-version".into() }
    } else {
        VersionSpec { spec: acro_cargo::DEFAULT_RUST.into(), source: "default".into() }
    };
    let rust = VersionSpec { spec: normalize_channel(&rust.spec), source: rust.source };
    let (bin, package) = if let Some((b, _)) = env.config("CARGO_BIN") {
        (b, None)
    } else if let Some(d) = &project.default_run {
        (d.clone(), None)
    } else if let Some(name) = &project.package_name {
        let b = if project.bins.is_empty() || project.bins.contains(name) {
            name.clone()
        } else {
            project.bins[0].clone()
        };
        (b, None)
    } else if !project.workspace_members.is_empty() {
        let mut found = None;
        for m in &project.workspace_members {
            let mdir = dir.join(m);
            if mdir.join("src/main.rs").exists()
                && let Ok(p) = acro_cargo::read_project(&mdir)
                && let Some(n) = p.package_name
            {
                found = Some(n);
                break;
            }
        }
        match found {
            Some(n) => (n.clone(), Some(n)),
            None => bail!("no binary crate found in the Cargo workspace"),
        }
    } else {
        bail!("Cargo.toml has no [package] or [workspace]");
    };
    Ok(RustApp { rust, project, bin, package, has_lock: dir.join("Cargo.lock").exists() })
}

fn normalize_channel(spec: &str) -> String {
    let s = spec.trim();
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 2 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return format!("{s}.0");
    }
    s.to_string()
}

pub fn plan(app: &RustApp, env: &Env, dir: &Path, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "rust");
    b.fact("rust", format!("{} ({})", app.rust.spec, app.rust.source));
    b.fact("binary", app.bin.clone());
    let glibc = acro_cargo::host_glibc();
    let base = acro_cargo::runtime_base_for_glibc(glibc)?;
    if let Some((a, c)) = glibc {
        b.fact("host-glibc", format!("{a}.{c}"));
    }
    b.step("base", format!("resolve {base}"), Action::ResolveBase { image: base.into() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "rust",
        format!("rust {}", app.rust.spec),
        Action::Toolchain { tool: "rust".into(), spec: app.rust.spec.clone(), parts: vec![] },
        &[],
    );
    let mut deps = vec!["rust"];
    let mut run_env = BTreeMap::new();
    for (k, v) in &env.vars {
        if !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_") {
            run_env.insert(k.clone(), v.clone());
        }
    }
    if app.has_lock {
        let lock = std::fs::read(dir.join("Cargo.lock"))?;
        b.step(
            "crates",
            "fetch crates",
            Action::CargoVendor { lockfile_sha256: acro_store::sha256_bytes(&lock).hex() },
            &[],
        );
        deps.push("crates");
    } else {
        b.plan.warnings.push("no Cargo.lock: dependencies are resolved by cargo with network access".into());
        b.step(
            "crates",
            "cargo fetch (no lockfile)",
            Action::Run {
                argv: vec!["cargo".into(), "fetch".into()],
                env: BTreeMap::new(),
                network: true,
                cwd: "@app".into(),
            },
            &["rust"],
        );
        deps.push("crates");
    }
    let mut argv: Vec<String> = vec!["cargo".into(), "build".into(), "--release".into()];
    if app.has_lock {
        argv.push("--locked".into());
        argv.push("--offline".into());
    }
    match &app.package {
        Some(p) => {
            argv.push("--package".into());
            argv.push(p.clone());
        }
        None => {
            argv.push("--bin".into());
            argv.push(app.bin.clone());
        }
    }
    b.step("build", format!("cargo build --release ({})", app.bin), Action::Run { argv, env: run_env, network: false, cwd: "@app".into() }, &deps);
    b.step(
        "layer-bin",
        "layer binary",
        Action::Layer {
            dest: format!("app/bin/{}", app.bin),
            from: LayerFrom::WorkFile { path: format!("{{work}}/target/release/{}", app.bin), mode: 0o755 },
        },
        &["build"],
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-bin"]);
    b.plan.image.layers = vec!["layer-bin".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(vec![format!("/app/bin/{}", app.bin)]);
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
