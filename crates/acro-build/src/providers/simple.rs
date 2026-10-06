use crate::detect::Env;
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub fn staticfile_root(dir: &Path, env: &Env) -> Option<(String, bool)> {
    let mut root: Option<String> = None;
    let mut fallback = false;
    if let Ok(text) = std::fs::read_to_string(dir.join("Staticfile")) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                match k.trim() {
                    "root" => root = Some(v),
                    "index_fallback" => fallback = v == "true",
                    _ => {}
                }
            }
        }
        if root.is_none() {
            root = Some(".".into());
        }
    }
    if let Some((r, _)) = env.config("STATIC_FILE_ROOT") {
        root = Some(r);
    }
    if root.is_none() && dir.join("index.html").exists() && !dir.join("package.json").exists() {
        root = Some(".".into());
    }
    root.map(|r| (r, fallback))
}

pub fn plan_static(dir: &Path, env: &Env, name: &str, root: &str, fallback: bool) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "staticfile");
    b.fact("root", root.to_string());
    let image = super::node::CADDY_IMAGE;
    b.step("base", format!("resolve {image}"), Action::ResolveBase { image: image.into() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    let mut files = BTreeMap::new();
    let custom = dir.join("Caddyfile");
    let caddyfile = if custom.exists() {
        std::fs::read_to_string(custom)?.replace("{{.DIST_DIR}}", "/app/dist")
    } else {
        super::node::spa_caddyfile("/app/dist", fallback)
    };
    let _ = env;
    files.insert("Caddyfile".to_string(), caddyfile);
    b.step("layer-caddy", "layer Caddyfile", Action::Layer { dest: "".into(), from: LayerFrom::Inline { files } }, &[]);
    let exclude = vec!["Staticfile".to_string(), "Caddyfile".to_string()];
    let from = if root == "." || root.is_empty() {
        LayerFrom::AppSource { exclude }
    } else {
        LayerFrom::AppSubdir { path: root.trim_start_matches("./").to_string(), exclude }
    };
    b.step("layer-site", format!("layer {root}"), Action::Layer { dest: "app/dist".into(), from }, &[]);
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-caddy", "layer-site"]);
    b.plan.image.layers = vec!["layer-caddy".into(), "layer-site".into()];
    b.plan.image.cmd = Some(vec!["caddy".into(), "run".into(), "--config".into(), "/Caddyfile".into(), "--adapter".into(), "caddyfile".into()]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.ports = vec![80];
    Ok(b.finish())
}

pub fn shell_script(dir: &Path, env: &Env) -> Option<String> {
    if let Some((s, _)) = env.config("SHELL_SCRIPT") {
        return Some(s);
    }
    ["start.sh"].iter().find(|f| dir.join(f).exists()).map(|s| s.to_string())
}

pub fn plan_shell(dir: &Path, name: &str, script: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "shell");
    b.fact("script", script.to_string());
    let first = std::fs::read_to_string(dir.join(script)).unwrap_or_default();
    let shebang = first.lines().next().unwrap_or("");
    let interp = if shebang.contains("bash") {
        "bash"
    } else if shebang.contains("zsh") {
        "zsh"
    } else {
        "sh"
    };
    let base = "debian:bookworm-slim";
    b.step("base", format!("resolve {base}"), Action::ResolveBase { image: base.into() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("layer-app", "layer app source", Action::Layer { dest: "app".into(), from: LayerFrom::AppSource { exclude: vec![] } }, &[]);
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-app"]);
    b.plan.image.layers = vec!["layer-app".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(vec![interp.into(), script.into()]);
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
