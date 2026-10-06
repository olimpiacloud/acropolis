use crate::detect::{Env, Framework, NodeApp, PackageManager};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

pub const NODE_PATH_ENV: &str = "/app/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const CADDY_IMAGE: &str = "caddy:2-alpine";

pub fn node_base_tag(spec: &str) -> Option<String> {
    let s = spec.trim().trim_start_matches('v');
    let lower = s.to_ascii_lowercase();
    if lower == "lts" || lower == "lts/*" {
        return Some("node:lts-bookworm-slim".into());
    }
    if lower == "latest" || lower == "current" || lower == "node" {
        return Some("node:current-bookworm-slim".into());
    }
    let parts: Vec<&str> = s.split('.').collect();
    if !parts.is_empty() && parts.len() <= 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
        return Some(format!("node:{s}-bookworm-slim"));
    }
    None
}

fn is_simple_command(s: &str) -> bool {
    let first = s.split_whitespace().next().unwrap_or("");
    !s.trim().is_empty()
        && !first.contains('=')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || " ._-/:@%+,".contains(c))
}

pub fn command_argv(cmd: &str) -> Vec<String> {
    if is_simple_command(cmd) {
        cmd.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        vec!["/bin/sh".into(), "-c".into(), cmd.to_string()]
    }
}

fn script_chain(app: &NodeApp, name: &str) -> Option<String> {
    let main = app.script(name)?;
    let mut parts = Vec::new();
    if let Some(pre) = app.script(&format!("pre{name}")) {
        parts.push(pre.to_string());
    }
    parts.push(main.to_string());
    if let Some(post) = app.script(&format!("post{name}")) {
        parts.push(post.to_string());
    }
    Some(parts.join(" && "))
}

fn uses_npm_cli(s: &str) -> bool {
    s.split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|' || c == '(')
        .any(|w| w == "npm" || w == "npx")
}

fn start_command(app: &NodeApp, env: &Env) -> Option<String> {
    if let Some((c, _)) = env.config("START_CMD") {
        return Some(c);
    }
    if let Some(s) = app.script("start") {
        return Some(s.to_string());
    }
    if let Some(m) = app.main()
        && app.dir.join(&m).exists()
    {
        return Some(format!("node {m}"));
    }
    for f in ["index.js", "server.js", "app.js", "main.js", "index.mjs", "server.mjs"] {
        if app.dir.join(f).exists() {
            return Some(format!("node {f}"));
        }
    }
    None
}

fn vite_out_dir(app: &NodeApp) -> String {
    for f in ["vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts"] {
        if let Ok(text) = std::fs::read_to_string(app.dir.join(f))
            && let Some(idx) = text.find("outDir")
        {
            let rest = &text[idx + 6..];
            let q = rest.find(['"', '\'', '`']);
            if let Some(q) = q {
                let quote = rest.as_bytes()[q] as char;
                let after = &rest[q + 1..];
                if let Some(end) = after.find(quote) {
                    let v = after[..end].trim_start_matches("./").to_string();
                    if !v.is_empty() {
                        return v;
                    }
                }
            }
        }
    }
    "dist".into()
}

fn next_standalone(app: &NodeApp) -> bool {
    for f in ["next.config.ts", "next.config.js", "next.config.mjs", "next.config.cjs"] {
        if let Ok(text) = std::fs::read_to_string(app.dir.join(f))
            && text.contains("standalone")
        {
            return true;
        }
    }
    false
}

fn tanstack_nitro(app: &NodeApp) -> bool {
    app.has_dep("nitro")
        && ["vite.config.ts", "vite.config.js"]
            .iter()
            .any(|f| std::fs::read_to_string(app.dir.join(f)).map(|t| t.contains("nitro")).unwrap_or(false))
}

enum Runtime {
    ServerNoBuild { start: String },
    ServerBuilt { start: String },
    NextStandalone,
    Nitro,
    Spa { out: String },
}

fn runtime(app: &NodeApp, env: &Env) -> Result<Runtime> {
    let start = start_command(app, env);
    let has_build = app.script("build").is_some() || env.config("BUILD_CMD").is_some();
    if let Some((dir, _)) = env.config("SPA_OUTPUT_DIR") {
        return Ok(Runtime::Spa { out: dir });
    }
    match app.framework {
        Framework::Next if next_standalone(app) && has_build => return Ok(Runtime::NextStandalone),
        Framework::TanstackStart if tanstack_nitro(app) && app.script("start").is_none() && has_build => {
            return Ok(Runtime::Nitro);
        }
        Framework::Vite if has_build && !env.flag("NO_SPA") => {
            let custom_start = app.script("start").map(|s| !s.contains("vite")).unwrap_or(false)
                || env.config("START_CMD").is_some();
            if !custom_start {
                return Ok(Runtime::Spa { out: vite_out_dir(app) });
            }
        }
        Framework::Cra if has_build && app.script("start").map(|s| s.contains("react-scripts")).unwrap_or(true) => {
            return Ok(Runtime::Spa { out: "build".into() });
        }
        _ => {}
    }
    let Some(start) = start else {
        bail!("no start command found: add a \"start\" script to package.json or set ACRO_START_CMD");
    };
    if has_build {
        Ok(Runtime::ServerBuilt { start })
    } else {
        Ok(Runtime::ServerNoBuild { start })
    }
}

pub fn plan(app: &NodeApp, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "node");
    b.fact("package-manager", app.pm.name());
    b.fact("node", format!("{} ({})", app.node.spec, app.node.source));
    b.fact("framework", format!("{:?}", app.framework).to_ascii_lowercase());
    let manager = match app.pm {
        PackageManager::Npm => "npm",
        PackageManager::Pnpm => "pnpm",
        PackageManager::Bun if app.lockfile.as_deref() == Some("bun.lock") => "bun",
        PackageManager::Yarn1 => "yarn",
        other => bail!("{} projects are not supported yet", other.name()),
    };
    let lockfile = app.lockfile.clone().unwrap_or_default();
    let lock_sha = if lockfile.is_empty() {
        b.plan.warnings.push("no lockfile: dependencies are resolved from the npm registry at build time, so the build is not reproducible".into());
        String::new()
    } else {
        acro_store::sha256_bytes(&std::fs::read(app.dir.join(&lockfile))?).hex()
    };
    let rt = runtime(app, env)?;
    let prod_opts = acro_npm::InstallOptions { include_dev: false, include_optional: true, platform: Default::default() };
    let prod_plan = if lockfile.is_empty() {
        acro_npm::InstallPlan::default()
    } else {
        crate::run::install_plan_for(manager, &app.dir, &lockfile, &prod_opts)?
    };
    let prod_scripts: Vec<String> = prod_plan
    .with_install_scripts()
    .iter()
    .map(|p| p.name.clone())
    .collect();
    if !prod_scripts.is_empty() {
        b.plan.warnings.push(format!(
            "packages with install scripts are not executed yet: {}",
            prod_scripts.join(", ")
        ));
    }
    let base_image = node_base_tag(&app.node.spec);
    let node_base = |b: &mut PlanBuilder| -> Result<()> {
        match base_image.clone() {
            Some(img) => b.step("base", format!("resolve {img}"), Action::ResolveBase { image: img }, &[]),
            None => b.step(
                "base",
                format!("resolve node {} base", app.node.spec),
                Action::ResolveNodeBase { spec: app.node.spec.clone(), variant: "bookworm-slim".into() },
                &[],
            ),
        };
        b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
        Ok(())
    };
    let mut image_env = vec![("NODE_ENV".to_string(), "production".to_string())];
    match rt {
        Runtime::ServerNoBuild { start } => {
            node_base(&mut b)?;
            b.step(
                "npm-fetch",
                "fetch production packages",
                Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: lock_sha, dev: false },
                &[],
            );
            b.step(
                "layer-deps",
                "layer node_modules (production)",
                Action::Layer { dest: "app".into(), from: LayerFrom::NodeModules { dev: false } },
                &["npm-fetch"],
            );
            b.step(
                "layer-app",
                "layer app source",
                Action::Layer {
                    dest: "app".into(),
                    from: LayerFrom::AppSource { exclude: vec!["**/node_modules".into()] },
                },
                &[],
            );
            b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-deps", "layer-app"]);
            image_env.push(("PATH".into(), NODE_PATH_ENV.into()));
            b.plan.image.layers = vec!["layer-deps".into(), "layer-app".into()];
            b.plan.image.cmd = Some(command_argv(&start));
            b.plan.image.entrypoint = Some(vec![]);
            b.plan.image.workdir = Some("/app".into());
            b.fact("runtime", "node server (no build step)");
        }
        rt => {
            let build_cmd = env
                .config("BUILD_CMD")
                .map(|(c, _)| c)
                .or_else(|| script_chain(app, "build"))
                .unwrap_or_default();
            let mut parts = vec![];
            if uses_npm_cli(&build_cmd) {
                parts.push("npm".to_string());
            }
            b.step(
                "node",
                format!("node {}", app.node.spec),
                Action::Toolchain { tool: "node".into(), spec: app.node.spec.clone(), parts },
                &[],
            );
            b.step(
                "npm-fetch",
                "fetch packages",
                Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: lock_sha, dev: true },
                &[],
            );
            b.step("source", "copy source", Action::CopySource { exclude: vec!["**/node_modules".into()] }, &[]);
            b.step("install", "install node_modules", Action::NpmInstall { dev: true }, &["npm-fetch", "source"]);
            let mut run_env = BTreeMap::new();
            run_env.insert("NODE_ENV".to_string(), "production".to_string());
            run_env.insert("NEXT_TELEMETRY_DISABLED".to_string(), "1".to_string());
            run_env.insert("npm_lifecycle_event".to_string(), "build".to_string());
            if let Some(n) = app.package_json.get("name").and_then(|n| n.as_str()) {
                run_env.insert("npm_package_name".to_string(), n.to_string());
            }
            for (k, v) in &env.vars {
                if !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_") {
                    run_env.insert(k.clone(), v.clone());
                }
            }
            let network = matches!(app.framework, Framework::Next) || env.flag("BUILD_NETWORK");
            if network {
                b.plan.warnings.push("the build step runs with network access (next build may fetch fonts); it is not hermetic".into());
            }
            b.step(
                "build",
                format!("run {build_cmd}"),
                Action::Run {
                    argv: vec!["/bin/sh".into(), "-c".into(), build_cmd.clone()],
                    env: run_env,
                    network,
                    cwd: ".".into(),
                },
                &["node", "install"],
            );
            match rt {
                Runtime::NextStandalone => {
                    node_base(&mut b)?;
                    b.step(
                        "layer-server",
                        "layer .next/standalone",
                        Action::Layer {
                            dest: "app".into(),
                            from: LayerFrom::WorkDir { path: ".next/standalone".into(), exclude: vec![] },
                        },
                        &["build"],
                    );
                    b.step(
                        "layer-static",
                        "layer .next/static + public",
                        Action::Layer {
                            dest: "app".into(),
                            from: LayerFrom::Paths {
                                items: vec![
                                    ("public".into(), "public".into()),
                                    (".next/static".into(), ".next/static".into()),
                                ],
                            },
                        },
                        &["build"],
                    );
                    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-server", "layer-static"]);
                    b.plan.image.layers = vec!["layer-server".into(), "layer-static".into()];
                    b.plan.image.cmd = Some(vec!["node".into(), "server.js".into()]);
                    image_env.push(("PORT".into(), "3000".into()));
                    image_env.push(("HOSTNAME".into(), "0.0.0.0".into()));
                    b.plan.image.ports = vec![3000];
                    b.fact("runtime", "next standalone server");
                }
                Runtime::Nitro => {
                    node_base(&mut b)?;
                    b.step(
                        "layer-server",
                        "layer .output",
                        Action::Layer {
                            dest: "app/.output".into(),
                            from: LayerFrom::WorkDir { path: ".output".into(), exclude: vec![] },
                        },
                        &["build"],
                    );
                    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-server"]);
                    b.plan.image.layers = vec!["layer-server".into()];
                    b.plan.image.cmd = Some(vec!["node".into(), ".output/server/index.mjs".into()]);
                    image_env.push(("PORT".into(), "3000".into()));
                    b.plan.image.ports = vec![3000];
                    b.fact("runtime", "nitro server");
                }
                Runtime::Spa { out } => {
                    b.step("base", format!("resolve {CADDY_IMAGE}"), Action::ResolveBase { image: CADDY_IMAGE.into() }, &[]);
                    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
                    let custom = ["Caddyfile", "Caddyfile.template"].iter().any(|f| app.dir.join(f).exists());
                    let mut files = BTreeMap::new();
                    let caddyfile = if custom {
                        std::fs::read_to_string(app.dir.join("Caddyfile"))
                            .or_else(|_| std::fs::read_to_string(app.dir.join("Caddyfile.template")))?
                            .replace("{{.DIST_DIR}}", "/app/dist")
                    } else {
                        spa_caddyfile("/app/dist", !env.config("SPA_INDEX_FALLBACK").map(|(v, _)| v == "false").unwrap_or(false))
                    };
                    files.insert("Caddyfile".to_string(), caddyfile);
                    b.step(
                        "layer-caddy",
                        "layer Caddyfile",
                        Action::Layer { dest: "".into(), from: LayerFrom::Inline { files } },
                        &[],
                    );
                    b.step(
                        "layer-site",
                        format!("layer {out}"),
                        Action::Layer { dest: "app/dist".into(), from: LayerFrom::WorkDir { path: out.clone(), exclude: vec![] } },
                        &["build"],
                    );
                    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-caddy", "layer-site"]);
                    b.plan.image.layers = vec!["layer-caddy".into(), "layer-site".into()];
                    b.plan.image.cmd = Some(vec![
                        "caddy".into(),
                        "run".into(),
                        "--config".into(),
                        "/Caddyfile".into(),
                        "--adapter".into(),
                        "caddyfile".into(),
                    ]);
                    b.plan.image.entrypoint = Some(vec![]);
                    b.plan.image.workdir = Some("/app".into());
                    b.plan.image.ports = vec![80];
                    b.fact("runtime", format!("static site from {out} served by caddy"));
                    b.plan.image.env = vec![];
                    return Ok(b.finish());
                }
                Runtime::ServerBuilt { start } => {
                    node_base(&mut b)?;
                    b.step(
                        "npm-fetch-prod",
                        "select production packages",
                        Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: String::new(), dev: false },
                        &["npm-fetch"],
                    );
                    b.step(
                        "layer-deps",
                        "layer node_modules (production)",
                        Action::Layer { dest: "app".into(), from: LayerFrom::NodeModules { dev: false } },
                        &["npm-fetch-prod"],
                    );
                    b.step(
                        "layer-app",
                        "layer app + build output",
                        Action::Layer {
                            dest: "app".into(),
                            from: LayerFrom::WorkDir {
                                path: ".".into(),
                                exclude: vec!["**/node_modules".into(), ".next/cache".into()],
                            },
                        },
                        &["build"],
                    );
                    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-deps", "layer-app"]);
                    b.plan.image.layers = vec!["layer-deps".into(), "layer-app".into()];
                    b.plan.image.cmd = Some(command_argv(&start));
                    image_env.push(("PATH".into(), NODE_PATH_ENV.into()));
                    b.fact("runtime", "node server (built)");
                }
                Runtime::ServerNoBuild { .. } => unreachable!(),
            }
            b.plan.image.entrypoint = Some(vec![]);
            b.plan.image.workdir = Some("/app".into());
        }
    }
    b.plan.image.env = image_env;
    Ok(b.finish())
}

pub fn spa_caddyfile(root: &str, fallback: bool) -> String {
    let fallback = if fallback { " /index.html" } else { "" };
    format!(
        "{{\n\tadmin off\n\tpersist_config off\n\tauto_https off\n\tlog {{\n\t\tformat json\n\t}}\n}}\n\n:{{$PORT:80}} {{\n\tlog {{\n\t\tformat json\n\t}}\n\trespond /health 200\n\theader {{\n\t\tX-Content-Type-Options \"nosniff\"\n\t\t-Server\n\t}}\n\troot * {root}\n\tfile_server {{\n\t\thide .git\n\t\thide .env*\n\t}}\n\tencode {{\n\t\tgzip\n\t\tzstd\n\t}}\n\ttry_files {{path}} {{path}}.html {{path}}/index.html{fallback}\n}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_tags() {
        assert_eq!(node_base_tag("22").unwrap(), "node:22-bookworm-slim");
        assert_eq!(node_base_tag("v23.5.0").unwrap(), "node:23.5.0-bookworm-slim");
        assert_eq!(node_base_tag("lts").unwrap(), "node:lts-bookworm-slim");
        assert!(node_base_tag(">=18").is_none());
    }

    #[test]
    fn argv() {
        assert_eq!(command_argv("node index.js"), vec!["node", "index.js"]);
        assert_eq!(command_argv("NODE_ENV=x node a.js")[0], "/bin/sh");
        assert_eq!(command_argv("node a.js && echo hi")[0], "/bin/sh");
    }
}
