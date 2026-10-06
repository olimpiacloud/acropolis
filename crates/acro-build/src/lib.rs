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
    if !dir.join("package.json").exists()
        && !dir.join("go.mod").exists()
        && !dir.join("Cargo.toml").exists()
        && let Some(script) = providers::simple::shell_script(dir, env)
    {
        return providers::simple::plan_shell(dir, &name, &script);
    }
    let app = detect::detect(dir, env)?;
    match &app {
        App::Node(n) => providers::node::plan(n, env, &name),
        App::Go(g) => providers::go::plan(g, env, &name),
        App::Rust(r) => providers::rust::plan(r, env, dir, &name),
    }
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
