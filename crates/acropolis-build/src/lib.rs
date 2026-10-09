pub mod cache;
pub mod config;
pub mod detect;
pub mod errors;
pub mod extend;
pub mod gc;
pub mod ignore;
pub mod plan;
pub mod providers;
pub mod run;
pub mod source;

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

pub use detect::{App, Env};
pub use plan::Plan;
pub use run::{BuildOptions, BuildResult};

pub fn app_name(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "app".to_string())
}

pub fn plan_app(dir: &Path, env: &Env) -> Result<Plan> {
    let mut env = env.clone();
    if let Some(cfg) = config::load(dir, &env)? {
        config::apply(&cfg, &mut env)?;
    }
    let mut plan = plan_provider(dir, &env)?;
    if env.vars.contains_key("ACROPOLIS_CUSTOM_STEPS") || env.config("DEPLOY_APT_PACKAGES").is_some() {
        providers::node::demote_distroless(&mut plan);
    }
    extend::apply_all(&mut plan, &env)?;
    Ok(plan)
}

fn plan_provider(dir: &Path, env: &Env) -> Result<Plan> {
    let name = app_name(dir);
    if let Some((root, fallback)) = providers::simple::staticfile_root(dir, env)
        && (env.config("STATIC_FILE_ROOT").is_some() || !detect::has_package_json(dir))
    {
        return providers::simple::plan_static(dir, env, &name, &root, fallback);
    }
    let forced = env.config("PROVIDER").map(|(p, _)| p);
    if (forced.as_deref() == Some("shell")
        || (forced.is_none()
            && !detect::has_package_json(dir)
            && !dir.join("go.mod").exists()
            && !dir.join("Cargo.toml").exists()))
        && let Some(script) = providers::simple::shell_script(dir, env)
    {
        let mut plan = providers::simple::plan_shell(dir, env, &name, &script)?;
        apply_runtime_packages(&mut plan, env)?;
        return Ok(plan);
    }
    if (forced.as_deref() == Some("php") || (forced.is_none() && !dir.join("go.mod").exists()))
        && providers::php::is_php(dir)
        && (forced.as_deref() == Some("php") || dir.join("composer.json").exists() || !detect::has_package_json(dir))
    {
        let mut plan = providers::php::plan(dir, env, &name)?;
        apply_deploy_apt(&mut plan, env)?;
        return Ok(plan);
    }
    if (forced.as_deref() == Some("ruby")
        || (forced.is_none() && !dir.join("go.mod").exists() && !dir.join("Cargo.toml").exists()))
        && providers::ruby::is_ruby(dir)
    {
        let mut plan = providers::ruby::plan(dir, env, &name)?;
        apply_mise_extras(&mut plan, dir, env);
        apply_runtime_packages(&mut plan, env)?;
        return Ok(plan);
    }
    if (forced.as_deref() == Some("python")
        || (forced.is_none() && !dir.join("go.mod").exists() && !dir.join("Cargo.toml").exists()))
        && providers::python::is_python(dir)
    {
        let mut plan = providers::python::plan(dir, env, &name)?;
        apply_mise_extras(&mut plan, dir, env);
        apply_runtime_packages(&mut plan, env)?;
        return Ok(plan);
    }
    let images = match forced.as_deref() {
        Some("node" | "go" | "rust" | "python" | "ruby" | "shell") => None,
        _ => providers::images::detect(dir, env),
    };
    // Railpack checks gleam and cpp after node: next to a package.json only java/elixir/deno/dotnet win.
    if let Some(spec) = images.filter(|s| {
        forced.is_some()
            || !detect::has_package_json(dir)
            || s.as_ref()
                .is_ok_and(|s| matches!(s.provider, "java" | "elixir" | "deno" | "dotnet"))
    }) {
        let mut plan = providers::images::plan(dir, env, &name, spec?)?;
        apply_runtime_packages(&mut plan, env)?;
        return Ok(plan);
    }
    let app = match detect::detect(dir, env) {
        Ok(app) => app,
        Err(e) => {
            if forced.is_none() && env.config("START_CMD").is_some() {
                let mut plan = providers::simple::plan_shell(dir, env, &name, "")?;
                plan.provider = "custom".into();
                apply_runtime_packages(&mut plan, env)?;
                return Ok(plan);
            }
            return Err(e);
        }
    };
    let mut plan = match &app {
        App::Node(n) => providers::node::plan(n, env, &name)?,
        App::Go(g) => providers::go::plan(g, env, &name)?,
        App::Rust(r) => providers::rust::plan(r, env, dir, &name)?,
    };
    apply_mise_extras(&mut plan, dir, env);
    apply_runtime_packages(&mut plan, env)?;
    Ok(plan)
}

/// App variables (without acropolis/railpack knobs) as `{env:NAME}` placeholders that build steps
/// resolve when they run, so values never land in the plan.
pub fn user_env(env: &Env) -> std::collections::BTreeMap<String, String> {
    env.vars
        .keys()
        .filter(|k| !k.starts_with("ACROPOLIS_") && !k.starts_with("RAILPACK_"))
        .map(|k| (k.clone(), format!("{{env:{k}}}")))
        .collect()
}

fn layers_tool(plan: &Plan, tool: &str) -> bool {
    plan.steps.iter().any(|s| {
        matches!(&s.action, plan::Action::Layer { from: plan::LayerFrom::Tool { tool: t, .. } | plan::LayerFrom::ToolTree { tool: t }, .. } if t == tool)
    })
}

fn apply_mise_extras(plan: &mut Plan, dir: &Path, env: &Env) {
    if !matches!(plan.provider.as_str(), "node" | "python" | "ruby") {
        return;
    }
    let mut base = match plan.step("base").map(|s| &s.action) {
        Some(plan::Action::ResolveBase { image }) => image.clone(),
        Some(plan::Action::ResolveNodeBase { .. }) => "node:".to_string(),
        _ => return,
    };
    if !providers::node::is_slim_runtime(plan)
        && (base.contains("distroless") || base.contains("alpine") || base.starts_with("caddy"))
    {
        return;
    }
    let mut extra_path: Vec<&str> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    for tool in ["node", "bun", "go", "python"] {
        let spec = detect::tool_version(dir, tool)
            .map(|v| v.spec)
            .or_else(|| plan.facts.get(&format!("extra-{tool}")).cloned())
            .or_else(|| {
                env.vars
                    .get(&format!("ACROPOLIS_{}_VERSION", tool.to_ascii_uppercase()))
                    .filter(|_| plan.provider != tool)
                    .cloned()
            });
        let Some(spec) = spec else { continue };
        let provided = match tool {
            "node" => base.starts_with("node:") || layers_tool(plan, "node"),
            "bun" => base.starts_with("oven/bun") || layers_tool(plan, "bun"),
            "go" => base.starts_with("golang:"),
            _ => base.starts_with("python:"),
        };
        if provided {
            continue;
        }
        if added.is_empty() && providers::node::is_slim_runtime(plan) {
            providers::node::demote_distroless(plan);
            base = match plan.step("base").map(|s| &s.action) {
                Some(plan::Action::ResolveBase { image }) => image.clone(),
                _ => "node:".to_string(),
            };
            if tool == "node" && base.starts_with("node:") {
                continue;
            }
        }
        let spec = if spec == "latest" { String::new() } else { spec };
        let toolchain = if tool == "python" { "python-standalone" } else { tool };
        let existing = plan
            .steps
            .iter()
            .find(|s| matches!(&s.action, plan::Action::Toolchain { tool: t, .. } if t == toolchain))
            .map(|s| s.id.clone());
        let tool_step = match existing {
            Some(id) => id,
            None => {
                let id = format!("mise-{tool}");
                plan.steps.insert(
                    0,
                    plan::Step {
                        id: id.clone(),
                        name: format!("{tool} {} (mise)", if spec.is_empty() { "latest" } else { &spec }),
                        action: plan::Action::Toolchain {
                            tool: toolchain.into(),
                            spec: spec.clone(),
                            parts: vec![],
                        },
                        deps: vec![],
                        hash: String::new(),
                    },
                );
                id
            }
        };
        let (dest, from) = match tool {
            "bun" => (
                "usr/local/bin",
                plan::LayerFrom::Tool {
                    tool: "bun".into(),
                    files: vec![("bin/bun".into(), "bun".into())],
                },
            ),
            "go" => {
                extra_path.push("/usr/local/go/bin");
                ("usr/local/go", plan::LayerFrom::ToolTree { tool: "go".into() })
            }
            "python" => {
                extra_path.push("/opt/python/bin");
                (
                    "opt/python",
                    plan::LayerFrom::ToolTree {
                        tool: "python-standalone".into(),
                    },
                )
            }
            _ => ("usr/local", plan::LayerFrom::ToolTree { tool: "node".into() }),
        };
        let layer_id = format!("layer-mise-{tool}");
        let push_idx = plan
            .steps
            .iter()
            .position(|s| s.id == "push")
            .unwrap_or(plan.steps.len());
        plan.steps.insert(
            push_idx,
            plan::Step {
                id: layer_id.clone(),
                name: format!("layer {tool} (mise)"),
                action: plan::Action::Layer {
                    dest: dest.into(),
                    from,
                },
                deps: vec![tool_step],
                hash: String::new(),
            },
        );
        if let Some(push) = plan.steps.iter_mut().find(|s| s.id == "push") {
            push.deps.push(layer_id.clone());
        }
        plan.image.layers.insert(0, layer_id);
        added.push(tool.to_string());
    }
    if added.is_empty() {
        return;
    }
    if !extra_path.is_empty() {
        let default = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string();
        match plan.image.env.iter_mut().find(|(k, _)| k == "PATH") {
            Some((_, v)) => *v = format!("{v}:{}", extra_path.join(":")),
            None => plan
                .image
                .env
                .push(("PATH".into(), format!("{default}:{}", extra_path.join(":")))),
        }
    }
    plan.facts.insert("mise-extras".into(), added.join(" "));
    plan.finalize();
}

fn apply_runtime_packages(plan: &mut Plan, env: &Env) -> Result<()> {
    if plan.facts.contains_key("runtime-packages") || env.config("DEPLOY_APT_PACKAGES").is_some() {
        providers::node::demote_distroless(plan);
    }
    let Some(pkgs) = plan.facts.get("runtime-packages").cloned() else {
        return apply_deploy_apt(plan, env);
    };
    let mut env2 = env.clone();
    let cur = env2.config("DEPLOY_APT_PACKAGES").map(|(v, _)| v).unwrap_or_default();
    let mut all: Vec<String> = cur
        .split([' ', ','])
        .chain(pkgs.split(' '))
        .filter(|p| !p.is_empty() && *p != "...")
        .map(|p| p.to_string())
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    all.retain(|p| seen.insert(p.clone()));
    env2.vars.insert("ACROPOLIS_DEPLOY_APT_PACKAGES".into(), all.join(" "));
    apply_deploy_apt(plan, &env2)
}

pub fn apply_deploy_apt(plan: &mut Plan, env: &Env) -> Result<()> {
    let Some((pkgs, _)) = env.config("DEPLOY_APT_PACKAGES") else {
        return Ok(());
    };
    let pkgs: Vec<String> = pkgs
        .split([' ', ','])
        .filter(|p| !p.is_empty())
        .map(|s| s.to_string())
        .collect();
    if pkgs.is_empty() {
        return Ok(());
    }
    let image = match plan.step("base").map(|s| &s.action) {
        Some(plan::Action::ResolveBase { image }) if image.starts_with('@') => "@base".to_string(),
        Some(plan::Action::ResolveBase { image }) => image.clone(),
        _ => anyhow::bail!("apt packages need a resolvable Debian base image"),
    };
    if image.contains("distroless") || image.contains("alpine") {
        anyhow::bail!("apt packages are not available on {image}");
    }
    let cmd = format!(
        "{}; apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb /var/log/apt /var/log/dpkg.log",
        providers::ruby::APT_ARCHIVE_FIX,
        pkgs.join(" ")
    );
    let push_idx = plan
        .steps
        .iter()
        .position(|s| s.id == "push")
        .unwrap_or(plan.steps.len());
    let apt = plan::Step {
        id: "apt".into(),
        name: format!("apt-get install {}", pkgs.join(" ")),
        deps: if image == "@base" { vec!["base".into()] } else { vec![] },
        action: plan::Action::ImageRun {
            image,
            commands: vec![cmd],
            env: Default::default(),
            network: true,
            mount_app: false,
            after: None,
            tools: vec![],
            lowers: vec![],
        },
        hash: String::new(),
    };
    let layer = plan::Step {
        id: "layer-apt".into(),
        name: "layer apt packages".into(),
        action: plan::Action::Layer {
            dest: String::new(),
            from: plan::LayerFrom::Upper {
                step: "apt".into(),
                include: vec![],
                exclude: vec!["var/cache".into(), "var/log".into(), "root".into()],
            },
        },
        deps: vec!["apt".into()],
        hash: String::new(),
    };
    plan.steps.insert(push_idx, layer);
    plan.steps.insert(push_idx, apt);
    if let Some(push) = plan.steps.iter_mut().find(|s| s.id == "push") {
        push.deps.push("layer-apt".into());
    }
    plan.image.layers.insert(0, "layer-apt".into());
    plan.facts.insert("apt-packages".into(), pkgs.join(" "));
    plan.finalize();
    Ok(())
}

pub async fn build(opts: BuildOptions, exec: Arc<dyn acropolis_exec::Executor>) -> Result<(Plan, BuildResult)> {
    let plan = plan_app(&opts.app_dir, &opts.env)?;
    let mut opts = opts;
    if let Some(cfg) = config::load(&opts.app_dir, &opts.env)? {
        config::apply(&cfg, &mut opts.env)?;
    }
    if let Some(c) = &plan.context {
        opts.app_dir = std::fs::canonicalize(opts.app_dir.join(c))?;
        acropolis_events::log(
            "plan",
            format!("workspace member: building from {}", opts.app_dir.display()),
        );
    }
    acropolis_events::emit(acropolis_events::Event::BuildStarted { app: plan.app.clone() });
    let res = run::execute(plan.clone(), opts, exec).await?;
    Ok((plan, res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_and_python_win_over_package_json_like_railpack() {
        let rust = detect::scratch_dir("precedence-rust");
        std::fs::create_dir_all(rust.join("src")).unwrap();
        std::fs::write(
            rust.join("Cargo.toml"),
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(rust.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(rust.join("package.json"), r#"{"name":"ws","private":true}"#).unwrap();
        assert_eq!(plan_app(&rust, &Env::default()).unwrap().provider, "rust");
        let mut node = Env::default();
        node.vars.insert("ACROPOLIS_PROVIDER".into(), "node".into());
        assert!(matches!(detect::detect(&rust, &node), Ok(App::Node(_))));

        let python = detect::scratch_dir("precedence-python");
        std::fs::write(python.join("requirements.txt"), "flask\n").unwrap();
        std::fs::write(python.join("main.py"), "print('hi')\n").unwrap();
        std::fs::write(
            python.join("package.json"),
            r#"{"devDependencies":{"tailwindcss":"4"}}"#,
        )
        .unwrap();
        assert_eq!(plan_app(&python, &Env::default()).unwrap().provider, "python");
        let _ = std::fs::remove_dir_all(&rust);
        let _ = std::fs::remove_dir_all(&python);
    }
}
