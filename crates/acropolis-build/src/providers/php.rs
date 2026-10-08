use crate::detect::{Env, tool_version};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_PHP: &str = "8.4";

pub fn is_php(dir: &Path) -> bool {
    dir.join("composer.json").exists() || dir.join("index.php").exists()
}

fn composer_json(dir: &Path) -> serde_json::Value {
    std::fs::read(dir.join("composer.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(serde_json::Value::Null)
}

fn version(dir: &Path, env: &Env) -> String {
    if let Some((v, _)) = env.config("PHP_VERSION") {
        return v;
    }
    if let Some(v) = tool_version(dir, "php") {
        return v.spec;
    }
    if let Some(r) = composer_json(dir).get("require").and_then(|r| r.get("php")).and_then(|v| v.as_str()) {
        let first = r.split("||").next().unwrap_or(r).trim();
        let f = acropolis_semver::fuzzy_version(first);
        if f.split('.').count() == 1 {
            return DEFAULT_PHP.into();
        }
        if !f.is_empty() && f != "latest" {
            return f.split('.').take(2).collect::<Vec<_>>().join(".");
        }
    }
    DEFAULT_PHP.into()
}

fn caddyfile(root: &str) -> String {
    format!(
        "{{\n\tadmin off\n\tpersist_config off\n\tauto_https off\n\t{{$CADDY_GLOBAL_OPTIONS}}\n\tlog {{\n\t\tformat json\n\t\toutput stderr\n\t}}\n\tfrankenphp {{\n\t\t{{$FRANKENPHP_CONFIG}}\n\t}}\n}}\n\n{{$CADDY_EXTRA_CONFIG}}\n\n:{{$PORT:80}} {{\n\troot * {root}\n\tencode zstd br gzip\n\tfile_server {{\n\t\thide .git\n\t\thide .env*\n\t}}\n\t{{$CADDY_SERVER_EXTRA_DIRECTIVES}}\n\tphp_server\n}}\n"
    )
}

const START: &str = "#!/bin/sh\nset -e\nif [ \"$IS_LARAVEL\" = \"true\" ]; then\n  if [ \"$ACROPOLIS_SKIP_MIGRATIONS\" != \"true\" ] && [ \"$RAILPACK_SKIP_MIGRATIONS\" != \"true\" ]; then\n    php artisan migrate --force\n  fi\n  php artisan storage:link || true\n  php artisan optimize:clear\n  php artisan optimize\nfi\nexec docker-php-entrypoint --config /Caddyfile --adapter caddyfile 2>&1\n";

pub fn plan(dir: &Path, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "php");
    let v = version(dir, env);
    let image = format!("dunglas/frankenphp:php{v}-trixie");
    let cj = composer_json(dir);
    let laravel = dir.join("artisan").exists() && cj.get("require").and_then(|r| r.get("laravel/framework")).is_some();
    b.fact("php", v.clone());
    if laravel {
        b.fact("framework", "laravel");
    }
    let mut exts: Vec<String> = cj
        .get("require")
        .and_then(|r| r.as_object())
        .map(|m| m.keys().filter_map(|k| k.strip_prefix("ext-").map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if let Some((e, _)) = env.config("PHP_EXTENSIONS") {
        exts.extend(e.split([',', ' ']).filter(|s| !s.is_empty()).map(|s| s.to_string()));
    }
    if laravel {
        for e in ["ctype", "curl", "dom", "fileinfo", "filter", "mbstring", "openssl", "pdo", "session", "tokenizer", "xml"] {
            exts.push(e.into());
        }
    }
    match env.vars.get("DB_CONNECTION").map(|s| s.as_str()) {
        Some("mysql") => exts.push("pdo_mysql".into()),
        Some("pgsql") => exts.push("pdo_pgsql".into()),
        _ => {}
    }
    exts.sort();
    exts.dedup();
    let builtin = ["ctype", "curl", "dom", "fileinfo", "filter", "hash", "mbstring", "openssl", "pcre", "pdo", "session", "tokenizer", "xml", "json", "pdo_sqlite", "sqlite3", "iconv", "libxml", "simplexml", "xmlreader", "xmlwriter", "phar", "posix", "readline", "sodium", "zlib", "spl", "standard", "date", "reflection"];
    let missing: Vec<String> = exts.into_iter().filter(|e| !builtin.contains(&e.as_str())).collect();
    b.step("base", format!("resolve {image}"), Action::ResolveBase { image: image.clone() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("source", "copy source", Action::CopySource { exclude: vec!["vendor".into(), "**/node_modules".into()] }, &[]);
    let mut commands = Vec::new();
    if !missing.is_empty() {
        commands.push(format!("install-php-extensions {}", missing.join(" ")));
    }
    let has_composer = dir.join("composer.json").exists();
    if has_composer {
        b.step("composer", "composer", Action::Toolchain { tool: "composer".into(), spec: String::new(), parts: vec![] }, &[]);
        commands.push(
            "command -v unzip >/dev/null || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends git zip unzip ca-certificates >/dev/null)"
                .into(),
        );
        commands.push("composer install --optimize-autoloader --no-scripts --no-interaction --no-dev".into());
    }
    if commands.is_empty() {
        commands.push("true".into());
    }
    let mut cenv = BTreeMap::new();
    cenv.insert("COMPOSER_ALLOW_SUPERUSER".to_string(), "1".to_string());
    cenv.insert("COMPOSER_HOME".to_string(), "/tmp/composer".to_string());
    let deps: Vec<&str> = if has_composer { vec!["source", "base", "composer"] } else { vec!["source", "base"] };
    b.step(
        "php-setup",
        "php extensions + composer install",
        Action::ImageRun {
            image: "@base".into(),
            commands,
            env: cenv,
            network: true,
            mount_app: true,
            after: None,
            tools: if has_composer { vec!["composer".into()] } else { vec![] },
            lowers: vec![],
        },
        &deps,
    );
    let mut app_dep = "php-setup".to_string();
    let pj = dir.join("package.json");
    if pj.exists() {
        let pjv: serde_json::Value = std::fs::read(&pj).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if pjv.get("scripts").and_then(|s| s.get("build")).is_some() && dir.join("package-lock.json").exists() {
            let node_spec = pjv.get("engines").and_then(|e| e.get("node")).and_then(|v| v.as_str()).unwrap_or("22").to_string();
            b.step("node", format!("node {node_spec}"), Action::Toolchain { tool: "node".into(), spec: node_spec, parts: vec!["npm".into()] }, &[]);
            let lock = std::fs::read(dir.join("package-lock.json"))?;
            b.step(
                "npm-fetch",
                "fetch packages",
                Action::NpmFetch { manager: "npm".into(), lockfile: "package-lock.json".into(), lockfile_sha256: acropolis_store::sha256_bytes(&lock).hex(), dev: true, workspaces: vec![], keep: vec![] },
                &[],
            );
            b.step("install", "install node_modules", Action::NpmInstall { dev: true, target: "src".into(), scripts: String::new(), manager: "npm".into(), types_only: false, patches: String::new() }, &["npm-fetch", "source"]);
            let mut aenv = BTreeMap::new();
            aenv.insert("NODE_ENV".to_string(), "production".to_string());
            aenv.insert("PATH".to_string(), "/app/node_modules/.bin:/opt/acropolis/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string());
            b.step(
                "assets",
                "npm run build (inside the PHP image)",
                Action::ImageRun {
                    image: "@base".into(),
                    commands: vec!["npm run build".into()],
                    env: aenv,
                    network: false,
                    mount_app: true,
                    after: Some("php-setup".into()),
                    tools: vec!["node".into()],
                    lowers: vec![],
                },
                &["php-setup", "install", "node"],
            );
            app_dep = "assets".into();
        }
    }
    b.step(
        "layer-system",
        "layer PHP extensions",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Upper { step: "php-setup".into(), include: vec![], exclude: vec!["var/cache".into(), "var/log".into(), "root".into()] },
        },
        &["php-setup"],
    );
    let mut files = BTreeMap::new();
    let root = env.config("PHP_ROOT_DIR").map(|(r, _)| r).unwrap_or_else(|| if laravel { "/app/public".into() } else { "/app".into() });
    files.insert("Caddyfile".to_string(), caddyfile(&root));
    files.insert("start-container.sh".to_string(), START.to_string());
    b.step("layer-config", "layer Caddyfile + start script", Action::Layer { dest: "".into(), from: LayerFrom::Inline { files } }, &[]);
    b.step(
        "layer-app",
        "layer app",
        Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec!["**/node_modules".into()] } },
        &[&app_dep],
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-system", "layer-config", "layer-app"]);
    b.plan.warnings.push("PHP extensions and composer packages are installed with network access inside the base image".into());
    b.plan.image.layers = vec!["layer-system".into(), "layer-config".into(), "layer-app".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.env = vec![
        ("APP_ENV".into(), "production".into()),
        ("APP_DEBUG".into(), "false".into()),
        ("LOG_CHANNEL".into(), "stderr".into()),
        ("SERVER_NAME".into(), ":80".into()),
        ("OCTANE_SERVER".into(), "frankenphp".into()),
        ("IS_LARAVEL".into(), laravel.to_string()),
    ];
    b.plan.image.cmd = Some(vec!["/start-container.sh".into()]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.ports = vec![80];
    Ok(b.finish())
}
