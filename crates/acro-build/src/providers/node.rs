use crate::detect::{Env, Framework, NodeApp, PackageManager};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;

pub const NODE_PATH_ENV: &str = "/app/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const CADDY_IMAGE: &str = "caddy:2-alpine";

pub fn node_base_tag(spec: &str) -> Option<String> {
    let fuzzy = acro_semver::fuzzy_version(spec);
    let s = fuzzy.as_str();
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
    let bun = app.pm == PackageManager::Bun;
    if bun {
        for f in ["index.ts", "index.tsx", "index.js", "server.ts", "src/index.ts", "app.ts"] {
            if app.dir.join(f).exists() {
                return Some(format!("bun {f}"));
            }
        }
    }
    for f in ["index.js", "server.js", "app.js", "main.js", "index.mjs", "server.mjs"] {
        if app.dir.join(f).exists() {
            return Some(format!("node {f}"));
        }
    }
    for f in ["index.ts", "server.ts", "src/index.ts"] {
        if app.dir.join(f).exists() {
            return Some(format!("bun {f}"));
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

fn read_first(app: &NodeApp, files: &[&str]) -> String {
    files.iter().find_map(|f| std::fs::read_to_string(app.dir.join(f)).ok()).unwrap_or_default()
}

fn config_string(text: &str, key: &str) -> Option<String> {
    let idx = text.find(key)?;
    let rest = &text[idx + key.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let q = rest.chars().next()?;
    if !matches!(q, '"' | '\'' | '`') {
        return None;
    }
    let after = &rest[1..];
    let end = after.find(q)?;
    Some(after[..end].trim_start_matches("./").trim_end_matches('/').to_string())
}

fn astro_server(app: &NodeApp) -> bool {
    let cfg = read_first(app, &["astro.config.mjs", "astro.config.ts", "astro.config.js", "astro.config.mts"]);
    cfg.contains("output: 'server'") || cfg.contains("output: \"server\"") || cfg.contains("output: 'hybrid'") || cfg.contains("adapter:")
}

fn react_router_spa(app: &NodeApp) -> bool {
    let cfg = read_first(app, &["react-router.config.ts", "react-router.config.js"]);
    cfg.contains("ssr: false") || cfg.contains("ssr:false")
}

fn angular_out(app: &NodeApp, env: &Env) -> Option<String> {
    let text = std::fs::read_to_string(app.dir.join("angular.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let projects = v.get("projects")?.as_object()?;
    let wanted = env.config("ANGULAR_PROJECT").map(|(p, _)| p);
    for (name, p) in projects {
        if wanted.as_ref().map(|w| w != name).unwrap_or(false) {
            continue;
        }
        let build = p.get("architect").and_then(|a| a.get("build"))?;
        let out = build.get("options").and_then(|o| o.get("outputPath")).and_then(|o| o.as_str()).unwrap_or("dist").to_string();
        let builder = build.get("builder").and_then(|b| b.as_str()).unwrap_or("");
        let browser = build.get("options").and_then(|o| o.get("browser")).is_some();
        if builder == "@angular-devkit/build-angular:application" || builder == "@angular/build:application" || browser {
            return Some(format!("{out}/browser"));
        }
        return Some(out);
    }
    None
}

fn expo_spa(app: &NodeApp) -> bool {
    if !app.has_dep("expo") || !app.has_dep("react-native-web") {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(app.dir.join("app.json")) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    let out = v.pointer("/expo/web/output").and_then(|o| o.as_str()).unwrap_or("").to_ascii_lowercase();
    out == "static" || out == "single"
}

fn runtime(app: &NodeApp, env: &Env) -> Result<Runtime> {
    let start = start_command(app, env);
    let has_build = app.script("build").is_some() || env.config("BUILD_CMD").is_some();
    if let Some((dir, _)) = env.config("SPA_OUTPUT_DIR") {
        return Ok(Runtime::Spa { out: dir });
    }
    let custom_start = env.config("START_CMD").is_some();
    let start_script = app.script("start").unwrap_or("").to_string();
    let default_start = |defaults: &[&str]| start_script.is_empty() || defaults.iter().any(|d| start_script.trim() == *d);
    if has_build && !custom_start && !env.flag("NO_SPA") {
        match app.framework {
            Framework::Vite if default_start(&["vite", "vite preview"]) => return Ok(Runtime::Spa { out: vite_out_dir(app) }),
            Framework::Cra if default_start(&["react-scripts start"]) => return Ok(Runtime::Spa { out: "build".into() }),
            Framework::Angular if default_start(&["ng serve"]) => {
                if let Some(out) = angular_out(app, env) {
                    return Ok(Runtime::Spa { out });
                }
            }
            Framework::Astro if !astro_server(app) && default_start(&["astro dev", "astro preview"]) => {
                let cfg = read_first(app, &["astro.config.mjs", "astro.config.ts", "astro.config.js", "astro.config.mts"]);
                return Ok(Runtime::Spa { out: config_string(&cfg, "outDir").unwrap_or_else(|| "dist".into()) });
            }
            Framework::Next if default_start(&["next start"]) => {
                let cfg = read_first(app, &["next.config.ts", "next.config.js", "next.config.mjs"]);
                if cfg.contains("output: 'export'") || cfg.contains("output: \"export\"") {
                    return Ok(Runtime::Spa { out: config_string(&cfg, "distDir").unwrap_or_else(|| "out".into()) });
                }
            }
            Framework::ReactRouter if react_router_spa(app) => {
                let cfg = read_first(app, &["react-router.config.ts", "react-router.config.js"]);
                let out = config_string(&cfg, "buildDirectory").unwrap_or_else(|| "build".into());
                return Ok(Runtime::Spa { out: format!("{out}/client") });
            }
            _ => {}
        }
        if expo_spa(app) {
            return Ok(Runtime::Spa { out: "dist".into() });
        }
    }
    match app.framework {
        Framework::Next if next_standalone(app) && has_build && !custom_start => return Ok(Runtime::NextStandalone),
        Framework::TanstackStart if tanstack_nitro(app) && app.script("start").is_none() && has_build => {
            return Ok(Runtime::Nitro);
        }
        Framework::Nuxt if app.script("start").is_none() && has_build && !custom_start => return Ok(Runtime::Nitro),
        _ => {}
    }
    let start = start.or_else(|| match app.framework {
        Framework::SvelteKit if has_build => Some("node build".into()),
        Framework::Astro if has_build && astro_server(app) => Some("node ./dist/server/entry.mjs".into()),
        Framework::ReactRouter if has_build => Some("react-router-serve ./build/server/index.js".into()),
        Framework::TanstackStart if has_build => Some("srvx --prod -s ../client dist/server/server.js".into()),
        _ => None,
    });
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
        PackageManager::Bun if app.lockfile.as_deref() == Some("bun.lockb") => return plan_bun_binary_lock(app, env, b),
        PackageManager::Yarn1 => "yarn",
        PackageManager::YarnBerry => return plan_yarn_berry(app, env, b),
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
    let policy = acro_npm::scripts::policy_for(manager, &app.package_json, env.config("INSTALL_SCRIPTS").map(|(v, _)| v).as_deref());
    let unknown_scripts = matches!(manager, "yarn" | "bun") || lockfile.is_empty();
    let prod_scripts: Vec<String> = prod_plan
        .with_install_scripts()
        .iter()
        .filter(|p| policy.allows(&p.name))
        .map(|p| p.name.clone())
        .collect();
    let prod_needs_scripts = !prod_scripts.is_empty() && policy != acro_npm::scripts::Policy::None;
    if prod_needs_scripts {
        b.fact("install-scripts", prod_scripts.join(", "));
        b.plan.warnings.push(format!(
            "install scripts of {} run with network access (policy: {}); set ACRO_INSTALL_SCRIPTS=none to disable",
            prod_scripts.join(", "),
            policy.describe()
        ));
    }
    let glibc_new = acro_cargo_glibc_newer_than_bookworm();
    let variant = if prod_needs_scripts && glibc_new { "trixie-slim" } else { "bookworm-slim" };
    let base_image = node_base_tag(&app.node.spec).map(|t| t.replace("bookworm-slim", variant));
    let node_base = |b: &mut PlanBuilder| -> Result<()> {
        match base_image.clone() {
            Some(img) => b.step("base", format!("resolve {img}"), Action::ResolveBase { image: img }, &[]),
            None => b.step(
                "base",
                format!("resolve node {} base", app.node.spec),
                Action::ResolveNodeBase { spec: app.node.spec.clone(), variant: variant.into() },
                &[],
            ),
        };
        b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
        Ok(())
    };
    let mut image_env = vec![("NODE_ENV".to_string(), "production".to_string())];
    match rt {
        Runtime::ServerNoBuild { start } => {
            let has_deps = ["dependencies", "optionalDependencies"]
                .iter()
                .any(|k| app.package_json.get(k).and_then(|d| d.as_object()).map(|m| !m.is_empty()).unwrap_or(false));
            if uses_bun(&start) && !has_deps && !start.contains("node ") {
                b.step("base", "resolve debian:bookworm-slim", Action::ResolveBase { image: "debian:bookworm-slim".into() }, &[]);
                b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
                b.fact("runtime", "bun (no node)");
            } else {
                node_base(&mut b)?;
            }
            b.step(
                "npm-fetch",
                "fetch production packages",
                Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: lock_sha, dev: false },
                &[],
            );
            prod_deps_layer(&mut b, app, manager, &policy, prod_needs_scripts, "npm-fetch", false);
            b.step(
                "layer-app",
                "layer app source",
                Action::Layer {
                    dest: "app".into(),
                    from: LayerFrom::AppSource { exclude: vec!["**/node_modules".into()] },
                },
                &[],
            );
            let mut push_deps = vec!["base", "copy-base", "layer-deps", "layer-app"];
            let mut layers = vec!["layer-deps".to_string(), "layer-app".to_string()];
            if uses_bun(&start) {
                add_bun_layer(&mut b, app, env);
                push_deps.push("layer-bun");
                layers.insert(0, "layer-bun".into());
            }
            b.step("push", "push image", Action::Push, &push_deps);
            image_env.push(("PATH".into(), NODE_PATH_ENV.into()));
            b.plan.image.layers = layers;
            b.plan.image.cmd = Some(command_argv(&start));
            b.plan.image.entrypoint = Some(vec![]);
            b.plan.image.workdir = Some("/app".into());
            if !b.plan.facts.contains_key("runtime") {
                b.fact("runtime", "node server (no build step)");
            }
        }
        rt => {
            let build_cmd = env
                .config("BUILD_CMD")
                .map(|(c, _)| c)
                .or_else(|| script_chain(app, "build"))
                .unwrap_or_default();
            let mut parts = vec![];
            let dev_opts = acro_npm::InstallOptions { include_dev: true, include_optional: true, platform: Default::default() };
            let dev_scripts = unknown_scripts
                || (!lockfile.is_empty()
                    && crate::run::install_plan_for(manager, &app.dir, &lockfile, &dev_opts)?
                        .with_install_scripts()
                        .iter()
                        .any(|p| policy.allows(&p.name)));
            let dev_scripts = dev_scripts && policy != acro_npm::scripts::Policy::None;
            if uses_npm_cli(&build_cmd) || dev_scripts || prod_needs_scripts {
                parts.push("npm".to_string());
            }
            if dev_scripts || prod_needs_scripts {
                parts.push("headers".to_string());
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
            let install_deps: &[&str] = if dev_scripts { &["npm-fetch", "source", "node"] } else { &["npm-fetch", "source"] };
            b.step(
                "install",
                "install node_modules",
                Action::NpmInstall {
                    dev: true,
                    target: "src".into(),
                    scripts: if dev_scripts { policy.describe() } else { String::new() },
                    manager: manager.into(),
                },
                install_deps,
            );
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
                        "/bin/sh".into(),
                        "-c".into(),
                        "exec caddy run --config /Caddyfile --adapter caddyfile 2>&1".into(),
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
                    prod_deps_layer(&mut b, app, manager, &policy, prod_needs_scripts, "npm-fetch-prod", true);
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
    add_package_manager(&mut b, app, env, &mut image_env);
    if app.framework == Framework::Astro {
        image_env.push(("HOST".into(), "0.0.0.0".into()));
    }
    b.plan.image.env = image_env;
    Ok(b.finish())
}

fn yarn_berry_version(app: &NodeApp) -> (String, Option<String>) {
    let rc = std::fs::read_to_string(app.dir.join(".yarnrc.yml")).unwrap_or_default();
    for line in rc.lines() {
        if let Some(p) = line.trim().strip_prefix("yarnPath:") {
            let p = p.trim().trim_matches('"').trim_matches('\'').to_string();
            let v = p.rsplit('/').next().unwrap_or("").trim_start_matches("yarn-").trim_end_matches(".cjs").trim_end_matches(".js").to_string();
            return (v, Some(p));
        }
    }
    if let Some(v) = &app.pm_version {
        return (v.clone(), None);
    }
    let lock = std::fs::read_to_string(app.dir.join("yarn.lock")).unwrap_or_default();
    let meta: u32 = lock
        .lines()
        .skip_while(|l| !l.starts_with("__metadata:"))
        .find_map(|l| l.trim().strip_prefix("version:").and_then(|v| v.trim().parse().ok()))
        .unwrap_or(8);
    let v = match meta {
        0..=4 => "2.4.3",
        5..=6 => "3.8.7",
        _ => "4",
    };
    (v.to_string(), None)
}

fn plan_yarn_berry(app: &NodeApp, env: &Env, mut b: PlanBuilder) -> Result<Plan> {
    let (yarn_version, yarn_path) = yarn_berry_version(app);
    b.fact("yarn", yarn_version.clone());
    let rt = runtime(app, env)?;
    b.step("node", format!("node {}", app.node.spec), Action::Toolchain { tool: "node".into(), spec: app.node.spec.clone(), parts: vec!["headers".into()] }, &[]);
    b.step("source", "copy source", Action::CopySource { exclude: vec!["**/node_modules".into()] }, &[]);
    let yarn_js = match &yarn_path {
        Some(p) => format!("{{src}}/{p}"),
        None => {
            b.step(
                "yarn",
                format!("yarn {yarn_version}"),
                Action::Toolchain { tool: "npm:@yarnpkg/cli-dist".into(), spec: yarn_version.clone(), parts: vec![] },
                &[],
            );
            "{tool:npm:@yarnpkg/cli-dist}/lib/node_modules/@yarnpkg/cli-dist/bin/yarn.js".to_string()
        }
    };
    let mut yenv = BTreeMap::new();
    yenv.insert("YARN_ENABLE_TELEMETRY".to_string(), "0".to_string());
    yenv.insert("YARN_ENABLE_GLOBAL_CACHE".to_string(), "0".to_string());
    yenv.insert("YARN_ENABLE_INLINE_BUILDS".to_string(), "1".to_string());
    let install_deps: Vec<&str> = if yarn_path.is_some() { vec!["node", "source"] } else { vec!["node", "source", "yarn"] };
    b.step(
        "install",
        "yarn install --immutable",
        Action::Run {
            argv: vec!["node".into(), yarn_js.clone(), "install".into(), "--immutable".into()],
            env: yenv.clone(),
            network: true,
            cwd: ".".into(),
        },
        &install_deps,
    );
    let mut last = "install".to_string();
    if app.script("build").is_some() || env.config("BUILD_CMD").is_some() {
        let mut benv = yenv.clone();
        benv.insert("NODE_ENV".into(), "production".into());
        let argv = match env.config("BUILD_CMD") {
            Some((c, _)) => vec!["/bin/sh".into(), "-c".into(), c],
            None => vec!["node".into(), yarn_js.clone(), "run".into(), "build".into()],
        };
        b.step("build", "yarn run build", Action::Run { argv, env: benv, network: false, cwd: ".".into() }, &["install"]);
        last = "build".into();
    }
    let pnp = !std::fs::read_to_string(app.dir.join(".yarnrc.yml")).unwrap_or_default().contains("nodeLinker: node-modules");
    let mut image_env = vec![("NODE_ENV".to_string(), "production".to_string())];
    match rt {
        Runtime::Spa { out } => {
            b.step("base", format!("resolve {CADDY_IMAGE}"), Action::ResolveBase { image: CADDY_IMAGE.into() }, &[]);
            b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
            let mut files = BTreeMap::new();
            files.insert("Caddyfile".to_string(), spa_caddyfile("/app/dist", true));
            b.step("layer-caddy", "layer Caddyfile", Action::Layer { dest: "".into(), from: LayerFrom::Inline { files } }, &[]);
            b.step("layer-site", format!("layer {out}"), Action::Layer { dest: "app/dist".into(), from: LayerFrom::WorkDir { path: out, exclude: vec![] } }, &[&last]);
            b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-caddy", "layer-site"]);
            b.plan.image.layers = vec!["layer-caddy".into(), "layer-site".into()];
            b.plan.image.cmd = Some(vec!["/bin/sh".into(), "-c".into(), "exec caddy run --config /Caddyfile --adapter caddyfile 2>&1".into()]);
            b.plan.image.workdir = Some("/app".into());
            b.plan.image.entrypoint = Some(vec![]);
            return Ok(b.finish());
        }
        Runtime::ServerNoBuild { start } | Runtime::ServerBuilt { start } => {
            let tag = node_base_tag(&app.node.spec).unwrap_or_else(|| "node:lts-bookworm-slim".into());
            b.step("base", format!("resolve {tag}"), Action::ResolveBase { image: tag }, &[]);
            b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
            b.step("layer-app", "layer app + dependencies", Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec![] } }, &[&last]);
            b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-app"]);
            b.plan.image.layers = vec!["layer-app".into()];
            b.plan.image.cmd = Some(command_argv(&start));
            if pnp {
                let mut opts = "--require /app/.pnp.cjs".to_string();
                if app.dir.join(".pnp.loader.mjs").exists() || yarn_version.split('.').next().and_then(|m| m.parse::<u32>().ok()).unwrap_or(4) >= 3 {
                    opts.push_str(" --experimental-loader /app/.pnp.loader.mjs");
                }
                image_env.push(("NODE_OPTIONS".into(), opts));
            }
            image_env.push(("PATH".into(), NODE_PATH_ENV.into()));
        }
        Runtime::NextStandalone | Runtime::Nitro => bail!("yarn berry with this framework is not supported yet"),
    }
    image_env.push(("npm_config_user_agent".into(), format!("yarn/{yarn_version} npm/? node/v{} linux x64", app.node.spec)));
    image_env.push(("npm_lifecycle_event".into(), "start".into()));
    b.plan.image.env = image_env;
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.warnings.push("yarn berry installs run the yarn CLI with network access".into());
    Ok(b.finish())
}

fn plan_bun_binary_lock(app: &NodeApp, env: &Env, mut b: PlanBuilder) -> Result<Plan> {
    let spec = bun_spec(app, env);
    let rt = runtime(app, env)?;
    b.step("bun", format!("bun {spec}"), Action::Toolchain { tool: "bun".into(), spec: spec.clone(), parts: vec![] }, &[]);
    b.step("node", format!("node {}", app.node.spec), Action::Toolchain { tool: "node".into(), spec: app.node.spec.clone(), parts: vec![] }, &[]);
    b.step("source", "copy source", Action::CopySource { exclude: vec!["**/node_modules".into()] }, &[]);
    b.step(
        "install",
        "bun install --frozen-lockfile",
        Action::Run { argv: vec!["bun".into(), "install".into(), "--frozen-lockfile".into()], env: BTreeMap::new(), network: true, cwd: ".".into() },
        &["bun", "node", "source"],
    );
    let mut last = "install";
    if let Some(build) = env.config("BUILD_CMD").map(|(c, _)| c).or_else(|| app.script("build").map(|_| "bun run build".to_string())) {
        let mut benv = BTreeMap::new();
        benv.insert("NODE_ENV".to_string(), "production".to_string());
        b.step("build", format!("run {build}"), Action::Run { argv: vec!["/bin/sh".into(), "-c".into(), build], env: benv, network: false, cwd: ".".into() }, &["install"]);
        last = "build";
    }
    let start = match rt {
        Runtime::ServerNoBuild { start } | Runtime::ServerBuilt { start } => start,
        _ => bail!("bun.lockb with this framework is not supported yet; migrate to the text bun.lock"),
    };
    let tag = node_base_tag(&app.node.spec).unwrap_or_else(|| "node:lts-bookworm-slim".into());
    b.step("base", format!("resolve {tag}"), Action::ResolveBase { image: tag }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "layer-bun",
        "layer bun binary",
        Action::Layer { dest: "usr/local/bin".into(), from: LayerFrom::Tool { tool: "bun".into(), files: vec![("bin/bun".into(), "bun".into())] } },
        &["bun"],
    );
    b.step("layer-app", "layer app + dependencies", Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec![] } }, &[last]);
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-bun", "layer-app"]);
    b.plan.image.layers = vec!["layer-bun".into(), "layer-app".into()];
    b.plan.image.cmd = Some(command_argv(&start));
    b.plan.image.env = vec![
        ("NODE_ENV".into(), "production".into()),
        ("PATH".into(), NODE_PATH_ENV.into()),
        ("npm_config_user_agent".into(), format!("bun/{spec} npm/? node/v{} linux x64", app.node.spec)),
    ];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.warnings.push("bun.lockb is installed by the bun CLI with network access".into());
    Ok(b.finish())
}

fn pnpm_spec(app: &NodeApp) -> String {
    if app.pm == PackageManager::Pnpm
        && let Some(v) = &app.pm_version
    {
        return v.clone();
    }
    if let Some(v) = app.package_json.get("engines").and_then(|e| e.get("pnpm")).and_then(|v| v.as_str()) {
        return acro_semver::fuzzy_version(v);
    }
    let lock = std::fs::read_to_string(app.dir.join("pnpm-lock.yaml")).unwrap_or_default();
    let ver = lock.lines().find_map(|l| l.strip_prefix("lockfileVersion:")).map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string());
    match ver.as_deref() {
        Some(v) if v.starts_with('5') => "7".into(),
        Some(v) if v.starts_with('6') => "8".into(),
        _ => "latest".into(),
    }
}

fn add_package_manager(b: &mut PlanBuilder, app: &NodeApp, env: &Env, image_env: &mut Vec<(String, String)>) {
    let _ = env;
    let uses_node_base = matches!(b.plan.step("base").map(|s| &s.action), Some(Action::ResolveNodeBase { .. }))
        || matches!(b.plan.step("base").map(|s| &s.action), Some(Action::ResolveBase { image }) if image.starts_with("node:"));
    if !uses_node_base {
        return;
    }
    let ua = match app.pm {
        PackageManager::Pnpm => {
            let spec = pnpm_spec(app);
            b.step("pnpm", format!("pnpm {spec}"), Action::Toolchain { tool: "npm:pnpm".into(), spec: spec.clone(), parts: vec![] }, &[]);
            b.step(
                "layer-pm",
                "layer pnpm CLI",
                Action::Layer { dest: "usr/local".into(), from: LayerFrom::ToolTree { tool: "npm:pnpm".into() } },
                &["pnpm"],
            );
            if let Some(push) = b.plan.steps.iter_mut().find(|s| s.id == "push") {
                push.deps.push("layer-pm".into());
            }
            let push_idx = b.plan.steps.iter().position(|s| s.id == "push").unwrap();
            let last = b.plan.steps.len() - 1;
            let tail = b.plan.steps.remove(push_idx);
            b.plan.steps.insert(last, tail);
            b.plan.image.layers.insert(0, "layer-pm".into());
            format!("pnpm/{spec} npm/? node/v{} linux x64", app.node.spec)
        }
        PackageManager::Yarn1 | PackageManager::YarnBerry => {
            format!("yarn/{} npm/? node/v{} linux x64", app.pm_version.clone().unwrap_or_else(|| "1.22.22".into()), app.node.spec)
        }
        PackageManager::Bun => format!("bun/{} npm/? node/v{} linux x64", app.pm_version.clone().unwrap_or_default(), app.node.spec),
        PackageManager::Npm => format!("npm/10 node/v{} linux x64", app.node.spec),
    };
    image_env.push(("npm_config_user_agent".into(), ua));
    image_env.push(("npm_lifecycle_event".into(), "start".into()));
    if let Some(n) = app.package_json.get("name").and_then(|n| n.as_str()) {
        image_env.push(("npm_package_name".into(), n.into()));
    }
}

fn uses_bun(cmd: &str) -> bool {
    cmd.split(|c: char| c.is_whitespace() || c == ';' || c == '&' || c == '|').any(|w| w == "bun" || w == "bunx")
}

fn bun_spec(app: &NodeApp, env: &Env) -> String {
    if let Some((v, _)) = env.config("BUN_VERSION") {
        return v;
    }
    if app.pm == PackageManager::Bun
        && let Some(v) = &app.pm_version
    {
        return v.clone();
    }
    if let Some(v) = app.package_json.get("engines").and_then(|e| e.get("bun")).and_then(|v| v.as_str()) {
        return v.to_string();
    }
    if let Some(v) = crate::detect::tool_version(&app.dir, "bun") {
        return v.spec;
    }
    "latest".into()
}

fn add_bun_layer(b: &mut PlanBuilder, app: &NodeApp, env: &Env) {
    let spec = bun_spec(app, env);
    if b.plan.step("bun").is_none() {
        b.step("bun", format!("bun {spec}"), Action::Toolchain { tool: "bun".into(), spec: spec.clone(), parts: vec![] }, &[]);
    }
    b.step(
        "layer-bun",
        "layer bun binary",
        Action::Layer {
            dest: "usr/local/bin".into(),
            from: LayerFrom::Tool { tool: "bun".into(), files: vec![("bin/bun".into(), "bun".into())] },
        },
        &["bun"],
    );
}

fn acro_cargo_glibc_newer_than_bookworm() -> bool {
    matches!(host_glibc(), Some(v) if v > (2, 36))
}

pub fn host_glibc() -> Option<(u32, u32)> {
    let out = std::process::Command::new("ldd").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.lines().next()?.split_whitespace().last()?.to_string();
    let mut it = ver.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn prod_deps_layer(
    b: &mut PlanBuilder,
    app: &NodeApp,
    manager: &str,
    policy: &acro_npm::scripts::Policy,
    scripts: bool,
    fetch_step: &str,
    has_node: bool,
) {
    if !scripts {
        b.step(
            "layer-deps",
            "layer node_modules (production)",
            Action::Layer { dest: "app".into(), from: LayerFrom::NodeModules { dev: false } },
            &[fetch_step],
        );
        return;
    }
    if !has_node {
        b.step(
            "node",
            format!("node {}", app.node.spec),
            Action::Toolchain { tool: "node".into(), spec: app.node.spec.clone(), parts: vec!["npm".into(), "headers".into()] },
            &[],
        );
    }
    b.step(
        "install-prod",
        "install production node_modules + scripts",
        Action::NpmInstall { dev: false, target: "prod".into(), scripts: policy.describe(), manager: manager.into() },
        &[fetch_step, "node"],
    );
    b.step(
        "layer-deps",
        "layer node_modules (production)",
        Action::Layer {
            dest: "app/node_modules".into(),
            from: LayerFrom::WorkDir { path: "@work/prod/node_modules".into(), exclude: vec![] },
        },
        &["install-prod"],
    );
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
