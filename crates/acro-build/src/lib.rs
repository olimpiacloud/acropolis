pub mod config;
pub mod detect;
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
    let env = &env;
    let name = app_name(dir);
    if let Some((root, fallback)) = providers::simple::staticfile_root(dir, env)
        && (dir.join("Staticfile").exists() || env.config("STATIC_FILE_ROOT").is_some() || !dir.join("package.json").exists())
    {
        return providers::simple::plan_static(dir, env, &name, &root, fallback);
    }
    let forced = env.config("PROVIDER").map(|(p, _)| p);
    if (forced.as_deref() == Some("shell")
        || (forced.is_none()
            && !dir.join("package.json").exists()
            && !dir.join("go.mod").exists()
            && !dir.join("Cargo.toml").exists()))
        && let Some(script) = providers::simple::shell_script(dir, env)
    {
        let mut plan = providers::simple::plan_shell(dir, env, &name, &script)?;
        apply_deploy_apt(&mut plan, env)?;
        return Ok(plan);
    }
    if (forced.as_deref() == Some("ruby") || (forced.is_none() && !dir.join("go.mod").exists() && !dir.join("Cargo.toml").exists()))
        && providers::ruby::is_ruby(dir)
    {
        let mut plan = providers::ruby::plan(dir, env, &name)?;
        apply_deploy_apt(&mut plan, env)?;
        return Ok(plan);
    }
    if (forced.as_deref() == Some("python")
        || (forced.is_none()
            && !dir.join("package.json").exists()
            && !dir.join("go.mod").exists()
            && !dir.join("Cargo.toml").exists()))
        && providers::python::is_python(dir)
    {
        let mut plan = providers::python::plan(dir, env, &name)?;
        apply_deploy_apt(&mut plan, env)?;
        return Ok(plan);
    }
    if forced.as_deref().map(|f| !matches!(f, "node" | "go" | "rust" | "python" | "ruby" | "shell")).unwrap_or(!dir.join("package.json").exists())
        && let Some(spec) = providers::images::detect(dir, env)
    {
        let mut plan = providers::images::plan(dir, env, &name, spec?)?;
        apply_deploy_apt(&mut plan, env)?;
        return Ok(plan);
    }
    let app = detect::detect(dir, env)?;
    let mut plan = match &app {
        App::Node(n) => providers::node::plan(n, env, &name)?,
        App::Go(g) => providers::go::plan(g, env, &name)?,
        App::Rust(r) => providers::rust::plan(r, env, dir, &name)?,
    };
    apply_deploy_apt(&mut plan, env)?;
    Ok(plan)
}

pub fn apply_deploy_apt(plan: &mut Plan, env: &Env) -> Result<()> {
    let Some((pkgs, _)) = env.config("DEPLOY_APT_PACKAGES") else { return Ok(()) };
    let pkgs: Vec<String> = pkgs.split([' ', ',']).filter(|p| !p.is_empty()).map(|s| s.to_string()).collect();
    if pkgs.is_empty() {
        return Ok(());
    }
    let image = match plan.step("base").map(|s| &s.action) {
        Some(plan::Action::ResolveBase { image }) => image.clone(),
        _ => anyhow::bail!("apt packages need a resolvable Debian base image"),
    };
    if image.contains("distroless") || image.contains("alpine") {
        anyhow::bail!("apt packages are not available on {image}");
    }
    let cmd = format!(
        "apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends {} && rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb /var/log/apt /var/log/dpkg.log",
        pkgs.join(" ")
    );
    let push_idx = plan.steps.iter().position(|s| s.id == "push").unwrap_or(plan.steps.len());
    let apt = plan::Step {
        id: "apt".into(),
        name: format!("apt-get install {}", pkgs.join(" ")),
        action: plan::Action::ImageRun { image, commands: vec![cmd], env: Default::default(), network: true, mount_app: false, after: None, tools: vec![] },
        deps: vec![],
        hash: String::new(),
    };
    let layer = plan::Step {
        id: "layer-apt".into(),
        name: "layer apt packages".into(),
        action: plan::Action::Layer {
            dest: String::new(),
            from: plan::LayerFrom::Upper { step: "apt".into(), include: vec![], exclude: vec!["var/cache".into(), "var/log".into(), "root".into()] },
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

pub async fn build(opts: BuildOptions, exec: Arc<dyn acro_exec::Executor>) -> Result<(Plan, BuildResult)> {
    let plan = plan_app(&opts.app_dir, &opts.env)?;
    let mut opts = opts;
    if let Some(cfg) = config::load(&opts.app_dir, &opts.env)? {
        config::apply(&cfg, &mut opts.env)?;
    }
    acro_events::emit(acro_events::Event::BuildStarted { app: plan.app.clone() });
    let res = run::execute(plan.clone(), opts, exec).await?;
    Ok((plan, res))
}
