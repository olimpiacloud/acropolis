use crate::detect::{Env, VersionSpec};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_RUST: &str = "1.89";

#[derive(Clone, Debug)]
pub struct RustApp {
    pub rust: VersionSpec,
    pub project: acropolis_cargo::RustProject,
    pub bin: String,
    pub package: Option<String>,
    pub has_lock: bool,
}

pub fn detect(dir: &Path, env: &Env) -> Result<RustApp> {
    let project = acropolis_cargo::read_project(dir)?;
    let edition_default = match project.edition.as_deref() {
        Some("2015") => Some("1.30.0"),
        Some("2018") => Some("1.55.0"),
        Some("2021") => Some("1.84.0"),
        Some("2024") => Some("1.85.1"),
        _ => None,
    };
    let rust = if let Some(v) = crate::detect::tool_version(dir, "rust") {
        v
    } else if let Some(c) = acropolis_cargo::toolchain_file(dir) {
        VersionSpec {
            spec: c,
            source: "rust-toolchain".into(),
        }
    } else if let Some(v) = project.rust_version.clone() {
        VersionSpec {
            spec: v,
            source: "Cargo.toml rust-version".into(),
        }
    } else if let Some(v) = ["rust-version.txt", ".rust-version"].iter().find_map(|f| {
        crate::detect::read_app_file(dir, f)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }) {
        VersionSpec {
            spec: v,
            source: ".rust-version".into(),
        }
    } else if let Some((v, k)) = env.config("RUST_VERSION") {
        VersionSpec { spec: v, source: k }
    } else if let Some(v) = edition_default {
        VersionSpec {
            spec: v.into(),
            source: "Cargo.toml edition".into(),
        }
    } else {
        VersionSpec {
            spec: DEFAULT_RUST.into(),
            source: "default".into(),
        }
    };
    let rust = VersionSpec {
        spec: normalize_channel(&rust.spec),
        source: rust.source,
    };
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
        let mut members = Vec::new();
        for m in &project.workspace_members {
            if let Some(base) = m.strip_suffix("/*") {
                let mut v: Vec<String> = std::fs::read_dir(dir.join(base))
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| e.path().join("Cargo.toml").exists())
                            .map(|e| format!("{base}/{}", e.file_name().to_string_lossy()))
                            .collect()
                    })
                    .unwrap_or_default();
                v.sort();
                members.extend(v);
            } else {
                members.push(m.clone());
            }
        }
        for m in &members {
            let mdir = dir.join(m);
            if mdir.join("src/main.rs").exists()
                && let Ok(p) = acropolis_cargo::read_project(&mdir)
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
    Ok(RustApp {
        rust,
        project,
        bin,
        package,
        has_lock: dir.join("Cargo.lock").exists(),
    })
}

fn normalize_channel(spec: &str) -> String {
    let s = spec.trim();
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() == 2 && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())) {
        return format!("{s}.0");
    }
    s.to_string()
}

const SYSTEM_SYS_CRATES: &[&str] = &[
    "openssl-sys",
    "libpq-sys",
    "pq-sys",
    "mysqlclient-sys",
    "libsqlite3-sys",
    "curl-sys",
    "libgit2-sys",
    "libssh2-sys",
    "zstd-sys",
    "rdkafka-sys",
    "librocksdb-sys",
];

/// Cargo.lock lists optional dependencies whatever features are on: every sqlx app locks `libsqlite3-sys` (and
/// `sqlx-mysql`) even with only `postgres`. These crates count only when a workspace manifest mentions them,
/// and a `bundled` build compiles the library from source, which the host can do.
const LOCKED_BY_DEFAULT: &[(&str, &[&str])] = &[("libsqlite3-sys", &["sqlite"])];

fn needs_system_libs(dir: &Path) -> bool {
    let lock = std::fs::read_to_string(dir.join("Cargo.lock")).unwrap_or_default();
    let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let manifests = workspace_manifests(dir);
    SYSTEM_SYS_CRATES.iter().any(|c| {
        let locked = lock.contains(&format!("name = \"{c}\"")) || toml.contains(c);
        match LOCKED_BY_DEFAULT.iter().find(|(name, _)| name == c) {
            Some((_, words)) => locked && words.iter().any(|w| manifests.contains(w)) && !manifests.contains("bundled"),
            None => locked,
        }
    }) && !toml.contains("vendored")
}

/// Text of every Cargo.toml in the app up to three levels deep (workspace members), skipping build outputs.
fn workspace_manifests(dir: &Path) -> String {
    fn walk(dir: &Path, depth: usize, out: &mut String) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name();
            if ft.is_file() && name == "Cargo.toml" {
                out.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
            } else if ft.is_dir()
                && depth > 0
                && !matches!(name.to_str(), Some("target" | "vendor" | "node_modules" | ".git"))
            {
                walk(&e.path(), depth - 1, out);
            }
        }
    }
    let mut out = String::new();
    walk(dir, 3, &mut out);
    out
}

fn plan_in_image(app: &RustApp, env: &Env, dir: &Path, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "rust");
    b.fact("rust", format!("{} ({})", app.rust.spec, app.rust.source));
    b.fact("binary", app.bin.clone());
    b.fact(
        "build",
        "inside rust image (system libraries needed by -sys crates)".to_string(),
    );
    let tag = match app.rust.spec.as_str() {
        "stable" | "latest" | "" => "1".to_string(),
        v => acropolis_semver::fuzzy_version(v),
    };
    let image = format!("rust:{}-bookworm", super::tag_part("Rust", &tag)?);
    let base = "gcr.io/distroless/cc-debian12";
    b.step(
        "base",
        format!("resolve {base}"),
        Action::ResolveBase { image: base.into() },
        &[],
    );
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "source",
        "copy source",
        Action::CopySource {
            exclude: vec!["target".into()],
        },
        &[],
    );
    let mut cmd = "cargo build --release".to_string();
    if dir.join("Cargo.lock").exists() {
        cmd.push_str(" --locked");
    }
    match &app.package {
        Some(p) => cmd.push_str(&format!(" --package {p}")),
        None => cmd.push_str(&format!(" --bin {}", app.bin)),
    }
    let mut run_env = BTreeMap::new();
    run_env.insert("CARGO_HOME".to_string(), "/app/.acropolis-cargo".to_string());
    run_env.extend(crate::user_env(env));
    b.step(
        "build",
        format!("{cmd} (in {image})"),
        Action::ImageRun {
            image,
            commands: vec![cmd],
            env: run_env,
            network: true,
            mount_app: true,
            after: None,
            tools: vec![],
            lowers: vec![],
        },
        &["source"],
    );
    b.step(
        "layer-bin",
        "layer binary",
        Action::Layer {
            dest: "app".into(),
            from: LayerFrom::Paths {
                items: vec![(format!("target/release/{}", app.bin), format!("bin/{}", app.bin))],
            },
        },
        &["build"],
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-bin"]);
    b.plan
        .warnings
        .push("crates with system libraries are built inside the rust image with network access".into());
    b.plan.image.layers = vec!["layer-bin".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(vec![format!("/app/bin/{}", app.bin)]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.env.push(("ROCKET_ADDRESS".into(), "0.0.0.0".into()));
    Ok(b.finish())
}

pub fn plan(app: &RustApp, env: &Env, dir: &Path, name: &str) -> Result<Plan> {
    if needs_system_libs(dir) {
        return plan_in_image(app, env, dir, name);
    }
    let mut b = PlanBuilder::new(name, "rust");
    b.fact("rust", format!("{} ({})", app.rust.spec, app.rust.source));
    b.fact("binary", app.bin.clone());
    let glibc = acropolis_cargo::host_glibc();
    let base = acropolis_cargo::runtime_base_for_glibc(glibc)?;
    if let Some((a, c)) = glibc {
        b.fact("host-glibc", format!("{a}.{c}"));
    }
    b.step(
        "base",
        format!("resolve {base}"),
        Action::ResolveBase { image: base.into() },
        &[],
    );
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "rust",
        format!("rust {}", app.rust.spec),
        Action::Toolchain {
            tool: "rust".into(),
            spec: app.rust.spec.clone(),
            parts: vec![],
        },
        &[],
    );
    let mut deps = vec!["rust"];
    let run_env = crate::user_env(env);
    if app.has_lock {
        let lock = std::fs::read(dir.join("Cargo.lock"))?;
        b.step(
            "crates",
            "fetch crates",
            Action::CargoVendor {
                lockfile_sha256: acropolis_store::sha256_bytes(&lock).hex(),
            },
            &[],
        );
        deps.push("crates");
    } else {
        b.plan
            .warnings
            .push("no Cargo.lock: dependencies are resolved by cargo with network access".into());
        // cargo writes Cargo.lock next to Cargo.toml: work on a copy so the build never changes the app directory.
        b.step("source", "copy source", Action::CopySource { exclude: vec![] }, &[]);
        b.step(
            "crates",
            "cargo fetch (no lockfile)",
            Action::Run {
                argv: vec!["cargo".into(), "fetch".into()],
                env: BTreeMap::new(),
                network: true,
                cwd: ".".into(),
            },
            &["rust", "source"],
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
    b.step(
        "build",
        format!("cargo build --release ({})", app.bin),
        Action::Run {
            argv,
            env: run_env,
            network: false,
            cwd: if app.has_lock { "@app" } else { "." }.into(),
        },
        &deps,
    );
    b.step(
        "layer-bin",
        "layer binary",
        Action::Layer {
            dest: format!("app/bin/{}", app.bin),
            from: LayerFrom::WorkFile {
                path: format!("{{cargo_target}}/release/{}", app.bin),
                mode: 0o755,
            },
        },
        &["build"],
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-bin"]);
    b.plan.image.layers = vec!["layer-bin".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(vec![format!("/app/bin/{}", app.bin)]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.env.push(("ROCKET_ADDRESS".into(), "0.0.0.0".into()));
    Ok(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_never_runs_in_the_app_dir_without_a_lockfile() {
        let dir = crate::detect::scratch_dir("rust-nolock");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let cwds = |p: &Plan| -> Vec<String> {
            p.steps
                .iter()
                .filter_map(|s| match &s.action {
                    Action::Run { cwd, .. } => Some(cwd.clone()),
                    _ => None,
                })
                .collect()
        };
        let p = crate::plan_app(&dir, &Env::default()).unwrap();
        assert_eq!(cwds(&p), [".", "."]);
        std::fs::write(dir.join("Cargo.lock"), "version = 4\n").unwrap();
        let p = crate::plan_app(&dir, &Env::default()).unwrap();
        assert_eq!(cwds(&p), ["@app"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn locked_sqlite_needs_the_system_library_only_when_used() {
        let dir = crate::detect::scratch_dir("rust-syslibs");
        std::fs::create_dir_all(dir.join("api")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"api\"]\n").unwrap();
        std::fs::write(
            dir.join("Cargo.lock"),
            "[[package]]\nname = \"libsqlite3-sys\"\nversion = \"0.30.1\"\n",
        )
        .unwrap();
        let api = |features: &str| {
            let toml = format!(
                "[package]\nname = \"api\"\n[dependencies]\nsqlx = {{ version = \"0.9\", features = [{features}] }}\n"
            );
            std::fs::write(dir.join("api/Cargo.toml"), toml).unwrap();
        };
        api("\"postgres\"");
        assert!(!needs_system_libs(&dir));
        api("\"sqlite\"");
        assert!(needs_system_libs(&dir));
        api("\"sqlite\", \"bundled\"");
        assert!(!needs_system_libs(&dir));
        std::fs::write(dir.join("Cargo.lock"), "[[package]]\nname = \"openssl-sys\"\n").unwrap();
        assert!(needs_system_libs(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
