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
    b.step(
        "base",
        format!("resolve {image}"),
        Action::ResolveBase { image: image.into() },
        &[],
    );
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
    b.step(
        "layer-caddy",
        "layer Caddyfile",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Inline { files },
        },
        &[],
    );
    let exclude = vec!["Staticfile".to_string(), "Caddyfile".to_string()];
    let from = if root == "." || root.is_empty() {
        LayerFrom::AppSource { exclude }
    } else {
        LayerFrom::AppSubdir {
            path: root.trim_start_matches("./").to_string(),
            exclude,
        }
    };
    b.step(
        "layer-site",
        format!("layer {root}"),
        Action::Layer {
            dest: "app/dist".into(),
            from,
        },
        &[],
    );
    b.step(
        "push",
        "push image",
        Action::Push,
        &["base", "copy-base", "layer-caddy", "layer-site"],
    );
    b.plan.image.layers = vec!["layer-caddy".into(), "layer-site".into()];
    b.plan.image.cmd = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "exec caddy run --config /Caddyfile --adapter caddyfile 2>&1".into(),
    ]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.ports = vec![80];
    Ok(b.finish())
}

pub fn shell_script(dir: &Path, env: &Env) -> Option<String> {
    if let Some((s, _)) = env.config("SHELL_SCRIPT") {
        return Some(s);
    }
    ["start.sh"]
        .iter()
        .find(|f| dir.join(f).exists())
        .map(|s| s.to_string())
}

pub fn plan_shell(dir: &Path, env: &Env, name: &str, script: &str) -> Result<Plan> {
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
    if interp == "zsh" {
        b.fact("runtime-packages", "zsh");
    }
    let base = "debian:bookworm-slim";
    b.step(
        "base",
        format!("resolve {base}"),
        Action::ResolveBase { image: base.into() },
        &[],
    );
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    let commands: Vec<String> = ["INSTALL_CMD", "BUILD_CMD"]
        .iter()
        .filter_map(|k| env.config(k).map(|(v, _)| v))
        .collect();
    let mut layers = Vec::new();
    let mut push_deps = vec!["base", "copy-base"];
    if commands.is_empty() {
        b.step(
            "layer-app",
            "layer app source",
            Action::Layer {
                dest: "app".into(),
                from: LayerFrom::AppSource { exclude: vec![] },
            },
            &[],
        );
    } else {
        b.step("source", "copy source", Action::CopySource { exclude: vec![] }, &[]);
        b.step(
            "build",
            format!("run {}", commands.join(" && ")),
            Action::ImageRun {
                image: base.into(),
                commands,
                env: BTreeMap::new(),
                network: false,
                mount_app: true,
                after: None,
                tools: vec![],
                lowers: vec![],
            },
            &["source"],
        );
        b.step(
            "layer-system",
            "layer system changes",
            Action::Layer {
                dest: "".into(),
                from: LayerFrom::Upper {
                    step: "build".into(),
                    include: vec![],
                    exclude: vec!["var/cache".into(), "var/log".into(), "root".into()],
                },
            },
            &["build"],
        );
        b.step(
            "layer-app",
            "layer app",
            Action::Layer {
                dest: "app".into(),
                from: LayerFrom::WorkDir {
                    path: ".".into(),
                    exclude: vec![],
                },
            },
            &["build"],
        );
        layers.push("layer-system".to_string());
        push_deps.push("layer-system");
    }
    layers.push("layer-app".into());
    push_deps.push("layer-app");
    b.step("push", "push image", Action::Push, &push_deps);
    b.plan.image.layers = layers;
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.cmd = Some(match env.config("START_CMD") {
        Some((c, _)) => vec!["/bin/sh".into(), "-c".into(), c],
        None => vec![interp.into(), script.into()],
    });
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
