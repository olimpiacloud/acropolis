use crate::detect::{Env, Framework, NodeApp, PackageManager};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use serde_json::Value;

pub const NODE_PATH_ENV: &str = "/app/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const DISTROLESS: &str = "distroless";
pub const DISTROLESS_NODE_MAJORS: &[u32] = &[22, 24, 26];
const NO_GYP_SCRIPTS: &[&str] = &[
    "esbuild",
    "sharp",
    "@swc/core",
    "core-js",
    "core-js-pure",
    "protobufjs",
    "nx",
    "prisma",
    "@prisma/client",
    "@prisma/engines",
    "@biomejs/biome",
    "@tailwindcss/oxide",
    "lefthook",
    "husky",
    "simple-git-hooks",
    "unrs-resolver",
    "msw",
    "es5-ext",
    "@scarf/scarf",
    "styled-components",
    "spawn-sync",
    "cypress",
    "puppeteer",
    "playwright",
    "workerd",
    "@sentry/cli",
    "vue-demi",
    "@nestjs/core",
    "turbo",
];
const BUN_BASE: &str = "gcr.io/distroless/cc-debian12:debug";
const SHIM: &str = "layer-shim";
pub const CADDY_IMAGE: &str = "caddy:2-alpine";

pub fn node_base_tag(spec: &str) -> Option<String> {
    let fuzzy = acropolis_semver::fuzzy_version(spec);
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
        && !first.contains('/')
        && !first.ends_with(".sh")
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

fn strip_js_comments(text: &str) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let b = text.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            out.push(c);
            if c == b'\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if matches!(c, b'"' | b'\'' | b'`') {
            quote = Some(c);
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn next_config(app: &NodeApp) -> Option<String> {
    ["next.config.ts", "next.config.mts", "next.config.js", "next.config.mjs", "next.config.cjs"]
        .iter()
        .find_map(|f| std::fs::read_to_string(app.dir.join(f)).ok())
        .map(|t| strip_js_comments(&t))
}

fn next_output(cfg: &str) -> Option<String> {
    let bytes = cfg.as_bytes();
    let mut from = 0;
    while let Some(off) = cfg[from..].find("output") {
        let idx = from + off;
        from = idx + 6;
        let before = idx.checked_sub(1).map(|i| bytes[i]);
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.') {
            continue;
        }
        let after = cfg[idx + 6..].trim_start();
        let after = after.strip_prefix(['"', '\'']).map(str::trim_start).unwrap_or(after);
        let Some(rest) = after.strip_prefix(':') else { continue };
        let rest = rest.trim_start();
        let q = rest.chars().next()?;
        if !matches!(q, '"' | '\'' | '`') {
            return Some(String::new());
        }
        return Some(rest[1..].split(q).next().unwrap_or("").to_string());
    }
    None
}

fn next_standalone(app: &NodeApp) -> bool {
    next_config(app).and_then(|c| next_output(&c)).as_deref() == Some("standalone")
}

fn next_major(app: &NodeApp) -> Option<u32> {
    ["dependencies", "devDependencies"]
        .iter()
        .find_map(|k| app.package_json.get(k).and_then(|d| d.get("next")).and_then(|v| v.as_str()))
        .and_then(|spec| spec.trim_start_matches(|c: char| !c.is_ascii_digit()).split('.').next().and_then(|m| m.parse::<u32>().ok()))
}

fn next_start_port(start: &str) -> Option<Option<String>> {
    let mut words = start.split_whitespace();
    if words.next()? != "next" || words.next()? != "start" {
        return None;
    }
    let mut port = None;
    while let Some(w) = words.next() {
        match w {
            "-p" | "--port" => port = Some(words.next()?.to_string()).filter(|p| !p.starts_with('$')),
            "-H" | "--hostname" => {
                words.next()?;
            }
            w if w.starts_with("--port=") => port = Some(w[7..].to_string()).filter(|p| !p.starts_with('$')),
            _ => return None,
        }
    }
    if port.as_deref().is_some_and(|p| !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some(port)
}

const NEXT_UNTRACEABLE: &[&str] = &[
    "dd-trace",
    "newrelic",
    "@opentelemetry/auto-instrumentations-node",
    "@sentry/profiling-node",
    "geoip-lite",
    "pdfkit",
    "@grpc/proto-loader",
    "pino",
    "next-i18next",
];

fn sharp_prebuilt(plan: &acropolis_npm::InstallPlan, version: &str) -> bool {
    let mut it = version.split(['.', '-']).map(|x| x.parse::<u32>().unwrap_or(0));
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    let cpu = acropolis_npm::Platform::default().cpu;
    (major, minor) >= (0, 33)
        && plan.packages.iter().any(|p| p.name == format!("@img/sharp-linux-{cpu}"))
        && plan.packages.iter().any(|p| p.name == format!("@img/sharp-libvips-linux-{cpu}"))
}

fn next_turbopack(app: &NodeApp, env: &Env, build_cmd: &str) -> Option<String> {
    if app.framework != Framework::Next || !env.flag("NEXT_TURBOPACK") || next_major(app) != Some(15) {
        return None;
    }
    let minor = ["dependencies", "devDependencies"]
        .iter()
        .find_map(|k| app.package_json.get(k).and_then(|d| d.get("next")).and_then(|v| v.as_str()))
        .and_then(|spec| spec.trim_start_matches(|c: char| !c.is_ascii_digit()).split('.').nth(1).and_then(|m| m.parse::<u32>().ok()))
        .unwrap_or(0);
    let words: Vec<&str> = build_cmd.split_whitespace().collect();
    if minor < 5 || words.len() < 2 || words[0] != "next" || words[1] != "build" || words.iter().any(|w| *w == "--turbopack" || *w == "--turbo" || *w == "--webpack") {
        return None;
    }
    if words[2..].iter().any(|w| !w.starts_with("--")) {
        return None;
    }
    Some(format!("{build_cmd} --turbopack"))
}

fn force_next_standalone(app: &NodeApp, env: &Env) -> Option<Option<String>> {
    if env.config("NEXT_STANDALONE").is_some_and(|(v, _)| v == "0" || v.eq_ignore_ascii_case("false"))
        || env.config("RUNTIME_BASE").is_some_and(|(v, _)| v.eq_ignore_ascii_case("debian"))
        || matches!(app.pm, PackageManager::YarnBerry)
        || app.lockfile.as_deref() == Some("bun.lockb")
        || NEXT_UNTRACEABLE.iter().any(|d| app.has_prod_dep(d))
    {
        return None;
    }
    match next_major(app) {
        Some(m) if m >= 15 => {}
        Some(13 | 14) if app.has_prod_dep("sharp") => {}
        _ => return None,
    }
    let build = app.script("build").unwrap_or("");
    let mut words = build.split_whitespace();
    if words.next() != Some("next") || words.next() != Some("build") || words.any(|w| !w.starts_with("--")) {
        return None;
    }
    let cfg = next_config(app).unwrap_or_default();
    if next_output(&cfg).is_some() || ["distDir", "RuntimeConfig", "PHASE_"].iter().any(|k| cfg.contains(k)) {
        return None;
    }
    next_start_port(app.script("start").unwrap_or("next start"))
}

fn nitro_start(start: Option<&str>) -> bool {
    match start {
        None => true,
        Some(s) => matches!(s.trim(), "node .output/server/index.mjs" | "node ./.output/server/index.mjs"),
    }
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
    NextStandalone { forced: bool, port: Option<String> },
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
                let cfg = next_config(app).unwrap_or_default();
                if next_output(&cfg).as_deref() == Some("export") {
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
        Framework::Next if next_standalone(app) && has_build && !custom_start => return Ok(Runtime::NextStandalone { forced: false, port: None }),
        Framework::Next if has_build && !custom_start && let Some(port) = force_next_standalone(app, env) => {
            return Ok(Runtime::NextStandalone { forced: true, port });
        }
        Framework::TanstackStart if tanstack_nitro(app) && nitro_start(app.script("start")) && has_build && !custom_start => {
            return Ok(Runtime::Nitro);
        }
        Framework::Nuxt if nitro_start(app.script("start")) && has_build && !custom_start => return Ok(Runtime::Nitro),
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
        bail!("no start command found: add a \"start\" script to package.json or set ACROPOLIS_START_CMD");
    };
    if has_build {
        Ok(Runtime::ServerBuilt { start })
    } else {
        Ok(Runtime::ServerNoBuild { start })
    }
}

const PUPPETEER_DEPS: &str = "xvfb libasound2 libatk1.0-0 libc6 libcairo2 libcups2 libdbus-1-3 libexpat1 libfontconfig1 libgbm1 libgcc1 libgdk-pixbuf-2.0-0 libglib2.0-0 libgtk-3-0 libnspr4 libpango-1.0-0 libpangocairo-1.0-0 libstdc++6 libx11-6 libx11-xcb1 libxcb1 libxcomposite1 libxcursor1 libxdamage1 libxext6 libxfixes3 libxi6 libxrandr2 libxrender1 libxss1 libxtst6 ca-certificates fonts-liberation libnss3 lsb-release xdg-utils wget";

const PLAYWRIGHT_DEPS: &str = "libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 libcairo2 libcups2 libdbus-1-3 libdrm2 libgbm1 libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2";

pub fn build_apt_packages(env: &Env) -> Option<String> {
    let (v, _) = env.config("BUILD_APT_PACKAGES")?;
    let pkgs: Vec<&str> = v.split([' ', ',']).filter(|p| !p.is_empty() && *p != "...").collect();
    (!pkgs.is_empty()).then(|| pkgs.join(" "))
}

fn pnpm_allowed_builds(app: &NodeApp) -> Vec<String> {
    let mut out: Vec<String> = app
        .package_json
        .pointer("/pnpm/onlyBuiltDependencies")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(app.root.join("pnpm-workspace.yaml"))
        && let Ok(y) = serde_yaml::from_str::<serde_yaml::Value>(&text)
    {
        if let Some(m) = y.get("allowBuilds").and_then(|m| m.as_mapping()) {
            for (k, v) in m {
                if let Some(k) = k.as_str()
                    && v.as_bool() != Some(false)
                {
                    out.push(k.to_string());
                }
            }
        }
        if let Some(a) = y.get("onlyBuiltDependencies").and_then(|a| a.as_sequence()) {
            out.extend(a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn pnpm_lock_lacks_build_info(app: &NodeApp, lockfile: &str) -> bool {
    if lockfile.is_empty() {
        return false;
    }
    let text = std::fs::read_to_string(app.root.join(lockfile)).unwrap_or_default();
    !text.contains("requiresBuild:")
}

fn playwright_install(app: &NodeApp, env: &Env) -> bool {
    env.flag("NODE_PLAYWRIGHT_INSTALL") && app.has_prod_dep("playwright")
}

fn browser_runtime(b: &mut PlanBuilder, app: &NodeApp, env: &Env, image_env: &mut Vec<(String, String)>) {
    let mut pkgs: Vec<&str> = Vec::new();
    if app.has_dep("puppeteer") {
        pkgs.extend(PUPPETEER_DEPS.split(' '));
        image_env.push(("PUPPETEER_CACHE_DIR".into(), "/app/node_modules/.cache/puppeteer".into()));
    }
    if playwright_install(app, env) {
        pkgs.extend(PLAYWRIGHT_DEPS.split(' '));
        image_env.push(("PLAYWRIGHT_BROWSERS_PATH".into(), "/app/node_modules/.cache/ms-playwright".into()));
    } else if app.has_prod_dep("playwright") {
        b.plan.warnings.push("playwright is a production dependency: set ACROPOLIS_NODE_PLAYWRIGHT_INSTALL=1 to install its headless browser".into());
    }
    if !pkgs.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        pkgs.retain(|p| seen.insert(*p));
        b.fact("runtime-packages", pkgs.join(" "));
    }
}

fn workspace_dirs(app: &NodeApp) -> Vec<String> {
    crate::detect::workspace_members(&app.dir, &app.package_json)
}

fn nx_next_app(app: &NodeApp, env: &Env) -> Option<(String, String)> {
    if !app.dir.join("nx.json").exists() || !app.has_dep("nx") {
        return None;
    }
    let mut apps = Vec::new();
    for rel in workspace_dirs(app) {
        let dir = app.dir.join(&rel);
        let pj: Value = std::fs::read(dir.join("package.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let has_config = ["next.config.js", "next.config.mjs", "next.config.ts", "next.config.cjs"].iter().any(|f| dir.join(f).exists());
        let has_next = ["dependencies", "devDependencies"].iter().any(|k| pj.get(k).and_then(|d| d.get("next")).is_some());
        if has_config || has_next {
            let name = pj.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()).unwrap_or_else(|| rel.rsplit('/').next().unwrap_or(&rel).to_string());
            apps.push((rel, name));
        }
    }
    if let Some((sel, _)) = env.config("NX_APP") {
        return apps.into_iter().find(|(rel, name)| {
            *name == sel || *rel == sel || rel.rsplit('/').next() == Some(sel.as_str()) || name.rsplit('/').next() == Some(sel.as_str())
        });
    }
    if apps.len() == 1 { apps.pop() } else { None }
}

pub fn plan(app: &NodeApp, env: &Env, name: &str) -> Result<Plan> {
    let mut p = plan_inner(app, env, name)?;
    if let Some(rel) = &app.member {
        apply_member(&mut p, app, rel);
    }
    Ok(p)
}

fn workspace_deps(app: &NodeApp, rel: &str) -> Vec<String> {
    let root_pj = crate::detect::read_package_json(&app.root).unwrap_or(Value::Null);
    let members = crate::detect::workspace_members(&app.root, &root_pj);
    let names: Vec<(String, String)> = members
        .iter()
        .filter_map(|m| {
            let pj = crate::detect::read_package_json(&app.root.join(m)).ok()?;
            Some((pj.get("name")?.as_str()?.to_string(), m.clone()))
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(pj) = crate::detect::read_package_json(&app.root.join(&dir)) else { continue };
        for key in ["dependencies", "optionalDependencies"] {
            for (dep, spec) in pj.get(key).and_then(|d| d.as_object()).into_iter().flatten() {
                let local = spec.as_str().map(|s| s.starts_with("workspace:")).unwrap_or(false) || names.iter().any(|(n, _)| n == dep);
                if !local {
                    continue;
                }
                if let Some((_, m)) = names.iter().find(|(n, _)| n == dep)
                    && m != rel
                    && !out.contains(m)
                {
                    out.push(m.clone());
                    stack.push(m.clone());
                }
            }
        }
    }
    out.sort();
    out
}

fn apply_member(plan: &mut Plan, app: &NodeApp, rel: &str) {
    let depth = rel.split('/').filter(|c| !c.is_empty()).count();
    plan.context = Some(vec![".."; depth].join("/"));
    plan.facts.insert("workspace-member".into(), rel.to_string());
    let deps = workspace_deps(app, rel);
    if !deps.is_empty() {
        plan.facts.insert("workspace-deps".into(), deps.join(" "));
    }
    let mut whole_tree = false;
    let mut extra_layers: Vec<(String, String)> = Vec::new();
    for s in plan.steps.iter_mut() {
        match &mut s.action {
            Action::NpmFetch { workspaces, .. } => *workspaces = vec![String::new(), rel.to_string()],
            Action::Layer { dest, from: LayerFrom::WorkDir { path, exclude } } if path == "." && dest == "app" => {
                whole_tree = true;
                *path = rel.to_string();
                *dest = format!("app/{rel}");
                if !exclude.iter().any(|e| e == "**/node_modules") {
                    exclude.push("**/node_modules".into());
                }
                extra_layers.push((s.id.clone(), s.deps.first().cloned().unwrap_or_default()));
            }
            Action::Run { cwd, .. } if cwd == "." => *cwd = rel.to_string(),
            Action::ImageRun { commands, mount_app: true, .. } => commands.insert(0, format!("cd /app/{rel}")),
            Action::Layer { from, .. } => match from {
                LayerFrom::WorkDir { path, .. } if path == "." => whole_tree = true,
                LayerFrom::WorkDir { path, .. } if !path.starts_with("@work/") => *path = format!("{rel}/{path}"),
                LayerFrom::AppSubdir { path, .. } => *path = format!("{rel}/{path}"),
                LayerFrom::AppSource { .. } => whole_tree = true,
                LayerFrom::Paths { items } => {
                    for (from, _) in items.iter_mut() {
                        *from = format!("{rel}/{from}");
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    for (layer_id, dep) in extra_layers {
        let mut items: Vec<(String, String)> = vec![("package.json".into(), "package.json".into())];
        items.extend(deps.iter().map(|d| (d.clone(), d.clone())));
        let id = format!("{layer_id}-workspace");
        let idx = plan.steps.iter().position(|s| s.id == layer_id).map(|i| i + 1).unwrap_or(plan.steps.len());
        plan.steps.insert(
            idx,
            crate::plan::Step {
                id: id.clone(),
                name: "layer workspace root and local packages".into(),
                action: Action::Layer { dest: "app".into(), from: LayerFrom::Paths { items } },
                deps: if dep.is_empty() { vec![] } else { vec![dep] },
                hash: String::new(),
            },
        );
        if let Some(push) = plan.steps.iter_mut().find(|s| s.id == "push") {
            push.deps.push(id.clone());
        }
        if let Some(pos) = plan.image.layers.iter().position(|l| *l == layer_id) {
            plan.image.layers.insert(pos, id);
        }
    }
    if plan.facts.get("runtime").map(|r| r.as_str()) == Some("next standalone server") {
        for s in plan.steps.iter_mut() {
            if s.id == "layer-static"
                && let Action::Layer { dest, .. } = &mut s.action
            {
                *dest = format!("app/{rel}");
            }
        }
        whole_tree = true;
    }
    if whole_tree && plan.image.workdir.as_deref() == Some("/app") {
        plan.image.workdir = Some(format!("/app/{rel}"));
    }
    for (k, v) in plan.image.env.iter_mut() {
        if k == "PATH" && v.contains("/app/node_modules/.bin") {
            *v = format!("/app/{rel}/node_modules/.bin:{v}");
        }
    }
    plan.finalize();
}

fn plan_inner(app: &NodeApp, env: &Env, name: &str) -> Result<Plan> {
    if app.script("build").is_none()
        && env.config("BUILD_CMD").is_none()
        && let Some((rel, project)) = nx_next_app(app, env)
    {
        let mut env = env.clone();
        env.vars.insert("ACROPOLIS_BUILD_CMD".into(), format!("nx build {project}"));
        if app.script("start").is_none() && env.config("START_CMD").is_none() {
            env.vars.insert("ACROPOLIS_START_CMD".into(), format!("cd {rel} && next start"));
        }
        env.vars.entry("NEXT_TELEMETRY_DISABLED".into()).or_insert_with(|| "1".into());
        let mut p = plan(app, &env, name)?;
        p.facts.insert("nx-app".into(), format!("{project} ({rel})"));
        return Ok(p);
    }
    let mut b = PlanBuilder::new(name, "node");
    b.fact("package-manager", app.pm.name());
    b.fact("node", format!("{} ({})", app.node.spec, app.node.source));
    b.fact("framework", format!("{:?}", app.framework).to_ascii_lowercase());
    let manager = match app.pm {
        PackageManager::Npm => "npm",
        PackageManager::Pnpm if app.lockfile.is_none() => "npm",
        PackageManager::Bun if app.lockfile.is_none() => "npm",
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
        acropolis_store::sha256_bytes(&std::fs::read(app.root.join(&lockfile))?).hex()
    };
    let rt = runtime(app, env)?;
    let prod_opts = acropolis_npm::InstallOptions { include_dev: false, include_optional: true, platform: Default::default() };
    let prod_plan = if lockfile.is_empty() {
        acropolis_npm::InstallPlan::default()
    } else {
        crate::run::install_plan_for(manager, &app.root, &lockfile, &prod_opts)?
    };
    let script_override = env.config("INSTALL_SCRIPTS").map(|(v, _)| v);
    let allowed_builds = if manager == "pnpm" { pnpm_allowed_builds(app) } else { Vec::new() };
    let policy = if script_override.is_none() && !allowed_builds.is_empty() {
        acropolis_npm::scripts::Policy::Only(allowed_builds.clone())
    } else {
        acropolis_npm::scripts::policy_for(manager, &app.package_json, script_override.as_deref())
    };
    let unknown_scripts = matches!(manager, "yarn" | "bun") || lockfile.is_empty();
    let guess_scripts = manager == "pnpm" && pnpm_lock_lacks_build_info(app, &lockfile);
    let scripted = |plan: &acropolis_npm::InstallPlan| -> Vec<String> {
        plan.packages
            .iter()
            .filter(|p| {
                p.has_install_script
                    || (guess_scripts
                        && (allowed_builds.contains(&p.name)
                            || (allowed_builds.is_empty() && acropolis_npm::scripts::BUN_DEFAULT_TRUSTED.contains(&p.name.as_str()))))
            })
            .filter(|p| policy.allows(&p.name))
            .filter(|p| !(p.name == "sharp" && sharp_prebuilt(plan, &p.version)))
            .map(|p| p.name.clone())
            .collect()
    };
    let mut prod_scripts: Vec<String> = scripted(&prod_plan);
    if lockfile.is_empty()
        && let Some(deps) = app.package_json.get("dependencies").and_then(|d| d.as_object())
    {
        prod_scripts.extend(
            deps.keys().filter(|k| acropolis_npm::scripts::BUN_DEFAULT_TRUSTED.contains(&k.as_str()) && policy.allows(k)).cloned(),
        );
    }
    let prod_needs_scripts = (!prod_scripts.is_empty() || playwright_install(app, env)) && policy != acropolis_npm::scripts::Policy::None;
    if prod_needs_scripts {
        b.fact("install-scripts", prod_scripts.join(", "));
        b.plan.warnings.push(format!(
            "install scripts of {} run with network access (policy: {}); set ACROPOLIS_INSTALL_SCRIPTS=none to disable",
            prod_scripts.join(", "),
            policy.describe()
        ));
    }
    let glibc_new = acropolis_cargo_glibc_newer_than_bookworm();
    let old_node = acropolis_semver::fuzzy_version(&app.node.spec).split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m < 20);
    let variant = if prod_needs_scripts && glibc_new && !old_node { "trixie-slim" } else { "bookworm-slim" };
    let base_image = node_base_tag(&app.node.spec).map(|t| t.replace("bookworm-slim", variant));
    let distroless = distroless_eligible(app, env, &rt, prod_needs_scripts);
    let bun_only = bun_only_runtime(app, env, &rt, prod_needs_scripts);
    let node_base = |b: &mut PlanBuilder| -> Result<()> {
        if bun_only {
            b.step("base", format!("resolve {BUN_BASE}"), Action::ResolveBase { image: BUN_BASE.into() }, &[]);
            b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
            b.step(SHIM, "layer sh and env links", Action::Layer { dest: String::new(), from: LayerFrom::NodeShim }, &["base"]);
            b.fact("runtime-base", "distroless cc + bun (busybox shell)");
            return Ok(());
        }
        if distroless {
            b.step(
                "base",
                format!("resolve node {} runtime (distroless)", app.node.spec),
                Action::ResolveNodeBase {
                    spec: app.node.spec.clone(),
                    variant: if variant == "trixie-slim" { format!("{DISTROLESS}-debian13") } else { DISTROLESS.into() },
                },
                &[],
            );
            b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
            b.step(SHIM, "layer sh, env and node links", Action::Layer { dest: String::new(), from: LayerFrom::NodeShim }, &["base"]);
            b.fact("runtime-base", "distroless node (busybox shell)");
            return Ok(());
        }
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
            if uses_bun(&start) && !has_deps && !start.contains("node ") && !bun_only {
                b.step("base", "resolve debian:bookworm-slim", Action::ResolveBase { image: "debian:bookworm-slim".into() }, &[]);
                b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
                b.fact("runtime", "bun (no node)");
            } else {
                node_base(&mut b)?;
                if bun_only {
                    b.fact("runtime", "bun (no node)");
                }
            }
            b.step(
                "npm-fetch",
                "fetch production packages",
                Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: lock_sha, dev: false, workspaces: vec![], keep: vec![] },
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
            let build_cmd = match next_turbopack(app, env, &build_cmd) {
                Some(cmd) => {
                    b.fact("next-bundler", "turbopack (ACROPOLIS_NEXT_TURBOPACK=1)");
                    cmd
                }
                None => build_cmd,
            };
            if let Runtime::Spa { out } = &rt
                && let Some(checks) = native_bundle_eligible(app, env, &build_cmd, &lockfile)
            {
                return plan_native_spa(app, env, b, manager, &lockfile, &lock_sha, out, checks);
            }
            let mut parts = vec![];
            let dev_opts = acropolis_npm::InstallOptions { include_dev: true, include_optional: true, platform: Default::default() };
            let dev_scripted = if lockfile.is_empty() { Vec::new() } else { scripted(&crate::run::install_plan_for(manager, &app.root, &lockfile, &dev_opts)?) };
            let dev_scripts = unknown_scripts || !dev_scripted.is_empty();
            let dev_scripts = dev_scripts && policy != acropolis_npm::scripts::Policy::None;
            if uses_npm_cli(&build_cmd) || dev_scripts || prod_needs_scripts {
                parts.push("npm".to_string());
            }
            let gyp_free = !unknown_scripts && dev_scripted.iter().chain(prod_scripts.iter()).all(|n| NO_GYP_SCRIPTS.contains(&n.as_str()));
            if (dev_scripts || prod_needs_scripts) && !gyp_free {
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
                Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: lock_sha, dev: true, workspaces: vec![], keep: vec![] },
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
                    types_only: false,
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
                if !k.starts_with("ACROPOLIS_") && !k.starts_with("RAILPACK_") {
                    run_env.insert(k.clone(), crate::env_ref(k, v));
                }
            }
            if matches!(rt, Runtime::NextStandalone { forced: true, .. }) {
                run_env.insert("NEXT_PRIVATE_STANDALONE".to_string(), "1".to_string());
            }
            let adapter_auto = app.framework == Framework::SvelteKit && app.has_dep("@sveltejs/adapter-auto");
            if adapter_auto {
                run_env.insert("GCP_BUILDPACKS".to_string(), "true".to_string());
            }
            if app.framework == Framework::Next && !env.flag("NO_NFT_CACHE") {
                let user = run_env.get("NODE_OPTIONS").cloned().map(|v| format!("{v} ")).unwrap_or_default();
                run_env.insert("NODE_OPTIONS".to_string(), format!("{user}--require {{work}}/acropolis-nft-cache.js"));
                run_env.insert("ACROPOLIS_NFT_CACHE".to_string(), "{src}/.next/cache/acropolis-nft-analysis.json".to_string());
            }
            let build_cmd = if adapter_auto { format!("{SVELTEKIT_ADAPTER_NODE} && {build_cmd}") } else { build_cmd };
            let network = !env.flag("HERMETIC_BUILD") || adapter_auto;
            if network {
                b.plan.warnings.push("the build step runs with network access like docker build; set ACROPOLIS_HERMETIC_BUILD=1 to run it without network".into());
            }
            if let Some(pkgs) = build_apt_packages(env) {
                b.fact("build-apt-packages", pkgs);
            }
            let scripts_use_bun = app
                .package_json
                .get("scripts")
                .and_then(|s| s.as_object())
                .is_some_and(|m| m.values().filter_map(|v| v.as_str()).any(uses_bun));
            let build_deps: Vec<&str> = if uses_bun(&build_cmd) || (app.pm == PackageManager::Bun && (scripts_use_bun || env.flag("BUN"))) {
                let spec = bun_spec(app, env);
                if b.plan.step("bun").is_none() {
                    b.step("bun", format!("bun {spec}"), Action::Toolchain { tool: "bun".into(), spec, parts: vec![] }, &[]);
                }
                vec!["node", "install", "bun"]
            } else {
                vec!["node", "install"]
            };
            b.step(
                "build",
                format!("run {build_cmd}"),
                match build_apt_packages(env) {
                    Some(pkgs) => {
                        let mut env = run_env;
                        env.insert("PATH".into(), "/app/node_modules/.bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
                        Action::ImageRun {
                            image: base_image.clone().unwrap_or_else(|| "node:lts-bookworm-slim".into()),
                            commands: vec![
                                format!("apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {pkgs} >/dev/null"),
                                build_cmd.clone(),
                            ],
                            env,
                            network: true,
                            mount_app: true,
                            after: None,
                            tools: vec!["node".into()],
                            lowers: vec![],
                        }
                    }
                    None => Action::Run { argv: vec!["/bin/sh".into(), "-c".into(), build_cmd.clone()], env: run_env, network, cwd: ".".into() },
                },
                &build_deps,
            );
            match rt {
                Runtime::NextStandalone { forced, port } => {
                    node_base(&mut b)?;
                    if forced {
                        b.step(
                            "layer-files",
                            "layer app files",
                            Action::Layer {
                                dest: "app".into(),
                                from: LayerFrom::WorkDir {
                                    path: ".".into(),
                                    exclude: vec!["**/node_modules".into(), ".next".into(), ".git".into(), "public".into()],
                                },
                            },
                            &["build"],
                        );
                    }
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
                    let mut layers = vec!["layer-server".to_string(), "layer-static".to_string()];
                    let mut push_deps = vec!["base", "copy-base", "layer-server", "layer-static"];
                    if forced {
                        layers.insert(0, "layer-files".into());
                        push_deps.push("layer-files");
                        b.fact("next-output", "standalone (set by acropolis; ACROPOLIS_NEXT_STANDALONE=0 to keep next start)");
                    }
                    b.step("push", "push image", Action::Push, &push_deps);
                    b.plan.image.layers = layers;
                    b.plan.image.cmd = Some(vec!["node".into(), "server.js".into()]);
                    image_env.push(("PORT".into(), port.clone().unwrap_or_else(|| "3000".into())));
                    image_env.push(("HOSTNAME".into(), "0.0.0.0".into()));
                    b.plan.image.ports = vec![port.as_deref().and_then(|p| p.parse().ok()).unwrap_or(3000)];
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
                        spa_caddyfile("/app/dist", spa_fallback(app, env))
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
                        Action::NpmFetch { manager: manager.into(), lockfile: lockfile.clone(), lockfile_sha256: String::new(), dev: false, workspaces: vec![], keep: runtime_dev_packages(app) },
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
                    let mut layers = vec!["layer-deps".to_string(), "layer-app".to_string()];
                    let mut push_deps = vec!["base", "copy-base", "layer-deps", "layer-app"];
                    if app.has_dep("@prisma/client") {
                        b.step(
                            "layer-prisma",
                            "layer generated prisma client",
                            Action::Layer {
                                dest: "app".into(),
                                from: LayerFrom::Paths { items: vec![("node_modules/.prisma".into(), "node_modules/.prisma".into())] },
                            },
                            &["build"],
                        );
                        layers.push("layer-prisma".into());
                        push_deps.push("layer-prisma");
                    }
                    if uses_bun(&start) {
                        add_bun_layer(&mut b, app, env);
                        push_deps.push("layer-bun");
                        layers.insert(0, "layer-bun".into());
                    }
                    b.step("push", "push image", Action::Push, &push_deps);
                    b.plan.image.layers = layers;
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
    if b.plan.step(SHIM).is_some() {
        b.plan.image.layers.insert(0, SHIM.into());
        if let Some(push) = b.plan.steps.iter_mut().find(|s| s.id == "push") {
            push.deps.push(SHIM.into());
        }
        if let Some((_, v)) = image_env.iter_mut().find(|(k, _)| k == "PATH") {
            v.push_str(":/busybox");
        }
    }
    add_package_manager(&mut b, app, env, &mut image_env);
    if manager == "pnpm" && b.plan.step("build").is_some() {
        if b.plan.step("pnpm").is_none() {
            let spec = pnpm_spec(app);
            b.step("pnpm", format!("pnpm {spec}"), Action::Toolchain { tool: "npm:pnpm".into(), spec, parts: vec![] }, &[]);
        }
        if let Some(build) = b.plan.steps.iter_mut().find(|s| s.id == "build")
            && !build.deps.iter().any(|d| d == "pnpm")
        {
            build.deps.push("pnpm".into());
        }
    }
    browser_runtime(&mut b, app, env, &mut image_env);
    if app.framework == Framework::Astro {
        image_env.push(("HOST".into(), "0.0.0.0".into()));
    }
    b.plan.image.env = image_env;
    Ok(b.finish())
}

fn yarn_berry_version(app: &NodeApp) -> (String, Option<String>) {
    let rc = std::fs::read_to_string(app.root.join(".yarnrc.yml")).unwrap_or_default();
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
    let lock = std::fs::read_to_string(app.root.join("yarn.lock")).unwrap_or_default();
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
    let major: u32 = yarn_version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(4);
    let yarn_js = match &yarn_path {
        Some(p) => format!("{{src}}/{p}"),
        None if major < 3 => {
            b.step(
                "yarn",
                format!("yarn {yarn_version}"),
                Action::Toolchain { tool: "yarn-berry".into(), spec: yarn_version.clone(), parts: vec![] },
                &[],
            );
            "{tool:yarn-berry}/yarn.js".to_string()
        }
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
    let pnp = !std::fs::read_to_string(app.root.join(".yarnrc.yml")).unwrap_or_default().contains("nodeLinker: node-modules");
    let mut image_env = vec![("NODE_ENV".to_string(), "production".to_string())];
    match rt {
        Runtime::Spa { out } => {
            b.step("base", format!("resolve {CADDY_IMAGE}"), Action::ResolveBase { image: CADDY_IMAGE.into() }, &[]);
            b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
            let mut files = BTreeMap::new();
            files.insert("Caddyfile".to_string(), spa_caddyfile_for(app, env)?);
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
            if pnp {
                b.plan.image.cmd = Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!(
                        "P=/app/.pnp.cjs; [ -f $P ] || P=/app/.pnp.js; export NODE_OPTIONS=\"--require $P${{NODE_OPTIONS:+ $NODE_OPTIONS}}\"; [ -f /app/.pnp.loader.mjs ] && export NODE_OPTIONS=\"$NODE_OPTIONS --experimental-loader /app/.pnp.loader.mjs\"; exec {start}"
                    ),
                ]);
            } else {
                b.plan.image.cmd = Some(command_argv(&start));
            }
            image_env.push(("PATH".into(), NODE_PATH_ENV.into()));
        }
        Runtime::NextStandalone { .. } | Runtime::Nitro => bail!("yarn berry with this framework is not supported yet"),
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

const SVELTEKIT_ADAPTER_NODE: &str = r#"{ [ -e node_modules/@sveltejs/adapter-node ] || { v=$(node -p "(require('fs').readFileSync('node_modules/@sveltejs/adapter-auto/adapters.js','utf8').match(/adapter-node['\"],\s*version:\s*['\"]([^'\"]+)/)||[0,'latest'])[1]" 2>/dev/null || echo latest) && d="${TMPDIR:-/tmp}/sveltekit-adapter-node" && npm install --prefix "$d" --no-save --no-package-lock --no-audit --no-fund --legacy-peer-deps --loglevel=error "@sveltejs/adapter-node@$v" && mkdir -p node_modules/@sveltejs && ln -sfn "$d/node_modules/@sveltejs/adapter-node" node_modules/@sveltejs/adapter-node; }; }"#;

fn native_bundle_eligible(app: &NodeApp, env: &Env, build_cmd: &str, lockfile: &str) -> Option<Vec<String>> {
    if app.member.is_some() {
        return None;
    }
    if app.framework != Framework::Vite || lockfile.is_empty() {
        return None;
    }
    if env.config("BUNDLER").map(|(v, _)| v == "vite").unwrap_or(false) || env.config("BUILD_CMD").is_some() {
        return None;
    }
    if !acropolis_bundle::simple_vite_config(&app.dir) {
        return None;
    }
    let mut checks = Vec::new();
    let mut saw_vite = false;
    for part in build_cmd.split("&&").map(|p| p.trim()) {
        if part == "vite build" {
            saw_vite = true;
        } else if !saw_vite && (part.starts_with("tsc") || part.starts_with("vue-tsc")) && !part.contains("--outDir") {
            checks.push(part.to_string());
        } else {
            return None;
        }
    }
    if saw_vite { Some(checks) } else { None }
}

fn types_only_check(app: &NodeApp, checks: &[String]) -> bool {
    if !checks.iter().all(|c| c == "tsc" || c.starts_with("tsc ")) {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(&app.dir) else { return false };
    !rd.flatten().any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.starts_with("tsconfig") && n.ends_with(".json") && std::fs::read_to_string(e.path()).map(|t| t.contains("maxNodeModuleJsDepth")).unwrap_or(true)
    })
}

#[allow(clippy::too_many_arguments)]
fn plan_native_spa(
    app: &NodeApp,
    env: &Env,
    mut b: PlanBuilder,
    manager: &str,
    lockfile: &str,
    lock_sha: &str,
    out: &str,
    checks: Vec<String>,
) -> Result<Plan> {
    b.fact("bundler", "acropolis (rolldown, lazy install)");
    b.step(
        "bundle",
        "bundle with lazy package fetch",
        Action::BundleSpa { manager: manager.into(), lockfile: lockfile.into(), out: out.into() },
        &[],
    );
    let mut site_deps = vec!["bundle".to_string()];
    if !checks.is_empty() {
        let check = checks.join(" && ");
        b.fact("type-check", check.clone());
        b.step("node", format!("node {}", app.node.spec), Action::Toolchain { tool: "node".into(), spec: app.node.spec.clone(), parts: vec![] }, &[]);
        b.step(
            "npm-fetch",
            "fetch packages",
            Action::NpmFetch { manager: manager.into(), lockfile: lockfile.into(), lockfile_sha256: lock_sha.into(), dev: true, workspaces: vec![], keep: vec![] },
            &[],
        );
        b.step("source", "copy source", Action::CopySource { exclude: vec!["**/node_modules".into()] }, &[]);
        b.step(
            "install",
            "install node_modules",
            Action::NpmInstall { dev: true, target: "src".into(), scripts: String::new(), manager: manager.into(), types_only: types_only_check(app, &checks) },
            &["npm-fetch", "source"],
        );
        let mut run_env = BTreeMap::new();
        run_env.insert("NODE_ENV".to_string(), "production".to_string());
        b.step(
            "typecheck",
            format!("run {check} (in parallel with the bundle)"),
            Action::Run { argv: vec!["/bin/sh".into(), "-c".into(), check], env: run_env, network: false, cwd: ".".into() },
            &["node", "install"],
        );
        site_deps.push("typecheck".into());
    }
    b.step("base", format!("resolve {CADDY_IMAGE}"), Action::ResolveBase { image: CADDY_IMAGE.into() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    let mut files = BTreeMap::new();
    files.insert("Caddyfile".to_string(), spa_caddyfile_for(app, env)?);
    b.step("layer-caddy", "layer Caddyfile", Action::Layer { dest: "".into(), from: LayerFrom::Inline { files } }, &[]);
    let deps: Vec<&str> = site_deps.iter().map(|s| s.as_str()).collect();
    b.step(
        "layer-site",
        format!("layer {out}"),
        Action::Layer { dest: "app/dist".into(), from: LayerFrom::WorkDir { path: format!("@work/bundle/{out}"), exclude: vec![] } },
        &deps,
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-caddy", "layer-site"]);
    b.plan.image.layers = vec!["layer-caddy".into(), "layer-site".into()];
    b.plan.image.cmd = Some(vec!["/bin/sh".into(), "-c".into(), "exec caddy run --config /Caddyfile --adapter caddyfile 2>&1".into()]);
    b.plan.image.entrypoint = Some(vec![]);
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.ports = vec![80];
    b.fact("runtime", format!("static site from {out} served by caddy"));
    Ok(b.finish())
}

fn pnpm_spec(app: &NodeApp) -> String {
    if app.pm == PackageManager::Pnpm
        && let Some(v) = &app.pm_version
    {
        return v.clone();
    }
    if let Some(v) = app.package_json.get("engines").and_then(|e| e.get("pnpm")).and_then(|v| v.as_str()) {
        return acropolis_semver::fuzzy_version(v);
    }
    let lock = std::fs::read_to_string(app.root.join("pnpm-lock.yaml")).unwrap_or_default();
    let ver = lock.lines().find_map(|l| l.strip_prefix("lockfileVersion:")).map(|v| v.trim().trim_matches('\'').trim_matches('"').to_string());
    match ver.as_deref() {
        Some(v) if v.starts_with('5') => "7".into(),
        Some(v) if v.starts_with('6') => "8".into(),
        _ => "9".into(),
    }
}

fn add_package_manager(b: &mut PlanBuilder, app: &NodeApp, env: &Env, image_env: &mut Vec<(String, String)>) {
    let _ = env;
    let uses_node_base = matches!(b.plan.step("base").map(|s| &s.action), Some(Action::ResolveNodeBase { .. }))
        || matches!(b.plan.step("base").map(|s| &s.action), Some(Action::ResolveBase { image }) if image.starts_with("node:") || image == BUN_BASE);
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
            image_env.push(("npm_config_verify_deps_before_run".into(), "false".into()));
            image_env.push(("pnpm_config_verify_deps_before_run".into(), "false".into()));
            image_env.push(("npm_config_update_notifier".into(), "false".into()));
            image_env.push(("pnpm_config_update_notifier".into(), "false".into()));
            image_env.push(("COREPACK_ENABLE_STRICT".into(), "0".into()));
            format!("pnpm/{{version:npm:pnpm|{spec}}} npm/? node/v{{version:node|{}}} linux x64", app.node.spec)
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

fn spa_caddyfile_for(app: &NodeApp, env: &Env) -> Result<String> {
    for f in ["Caddyfile", "Caddyfile.template"] {
        if let Ok(t) = std::fs::read_to_string(app.dir.join(f)) {
            return Ok(t.replace("{{.DIST_DIR}}", "/app/dist"));
        }
    }
    Ok(spa_caddyfile("/app/dist", spa_fallback(app, env)))
}

fn spa_fallback(app: &NodeApp, env: &Env) -> bool {
    if let Some((v, _)) = env.config("SPA_INDEX_FALLBACK") {
        return v != "false";
    }
    if app.dir.join("Staticfile").exists() {
        let text = std::fs::read_to_string(app.dir.join("Staticfile")).unwrap_or_default();
        if let Some(v) = text.lines().find_map(|l| l.trim().strip_prefix("index_fallback:")) {
            return v.trim() == "true";
        }
    }
    true
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
    if let Ok(t) = std::fs::read_to_string(app.dir.join(".bun-version"))
        && let Some(v) = t.lines().next().map(|l| l.trim().trim_start_matches('v').to_string()).filter(|v| !v.is_empty())
    {
        return v;
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

fn acropolis_cargo_glibc_newer_than_bookworm() -> bool {
    matches!(host_glibc(), Some(v) if v > (2, 36))
}

pub fn host_glibc() -> Option<(u32, u32)> {
    let out = std::process::Command::new("ldd").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ver = text.lines().next()?.split_whitespace().last()?.to_string();
    let mut it = ver.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn slim_runtime_allowed(app: &NodeApp, env: &Env) -> bool {
    !(env.config("RUNTIME_BASE").is_some_and(|(v, _)| v.eq_ignore_ascii_case("debian"))
        || app.has_dep("puppeteer")
        || app.has_prod_dep("playwright")
        || env.config("DEPLOY_APT_PACKAGES").is_some()
        || env.vars.contains_key("ACROPOLIS_CUSTOM_STEPS"))
}

fn bun_only_start(app: &NodeApp, start: &str, depth: usize) -> bool {
    let words: Vec<&str> = start.split_whitespace().collect();
    if depth > 4 || !is_simple_command(start) || words.first() != Some(&"bun") {
        return false;
    }
    match words.get(1).copied() {
        Some("run") => match words.get(2).and_then(|w| app.script(w)) {
            Some(script) => bun_only_start(app, script, depth + 1),
            None => words.get(2).is_some_and(|w| w.contains('.') || w.contains('/')),
        },
        Some(w) => !w.starts_with('-') && (w.contains('.') || w.contains('/')),
        None => false,
    }
}

fn bun_only_runtime(app: &NodeApp, env: &Env, rt: &Runtime, prod_needs_scripts: bool) -> bool {
    let start = match rt {
        Runtime::ServerBuilt { start } | Runtime::ServerNoBuild { start } => start,
        _ => return false,
    };
    slim_runtime_allowed(app, env)
        && !prod_needs_scripts
        && !matches!(app.pm, PackageManager::Pnpm | PackageManager::Yarn1 | PackageManager::YarnBerry)
        && bun_only_start(app, start, 0)
}

fn floating_node_spec(spec: &str) -> bool {
    let s = spec.trim().trim_start_matches('v').to_ascii_lowercase();
    if matches!(s.as_str(), "lts" | "lts/*" | "latest" | "current" | "node" | "*" | "") {
        return true;
    }
    let major_only = |t: &str| {
        let t = t.trim().trim_start_matches('v');
        let mut parts = t.split('.');
        let first = parts.next().unwrap_or("");
        !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) && parts.all(|p| matches!(p, "x" | "X" | "*" | "0"))
    };
    if let Some(rest) = s.strip_prefix(">=") {
        return major_only(rest);
    }
    if let Some(rest) = s.strip_prefix('^') {
        return major_only(rest);
    }
    let mut parts = s.split('.');
    let first = parts.next().unwrap_or("");
    !first.is_empty() && first.chars().all(|c| c.is_ascii_digit()) && parts.all(|p| matches!(p, "x" | "*"))
}

fn distroless_eligible(app: &NodeApp, env: &Env, rt: &Runtime, prod_needs_scripts: bool) -> bool {
    if !slim_runtime_allowed(app, env) || !floating_node_spec(&app.node.spec) {
        return false;
    }
    match rt {
        Runtime::Nitro | Runtime::NextStandalone { .. } => true,
        Runtime::ServerBuilt { start } | Runtime::ServerNoBuild { start } => {
            let first = start.split_whitespace().next().unwrap_or("");
            !prod_needs_scripts
                && is_simple_command(start)
                && !uses_bun(start)
                && !matches!(first, "npm" | "npx" | "yarn" | "pnpm" | "pnpx" | "corepack" | "sh" | "bash")
        }
        Runtime::Spa { .. } => false,
    }
}

pub fn is_slim_runtime(plan: &Plan) -> bool {
    match plan.step("base").map(|s| &s.action) {
        Some(Action::ResolveNodeBase { variant, .. }) => variant.starts_with(DISTROLESS),
        Some(Action::ResolveBase { image }) => image == BUN_BASE,
        _ => false,
    }
}

pub fn demote_distroless(plan: &mut Plan) {
    let fact_spec = plan.facts.get("node").and_then(|f| f.split(" (").next()).unwrap_or("lts").to_string();
    let Some(base) = plan.steps.iter_mut().find(|s| s.id == "base") else { return };
    let spec = match &base.action {
        Action::ResolveNodeBase { spec, variant } if variant.starts_with(DISTROLESS) => spec.clone(),
        Action::ResolveBase { image } if image == BUN_BASE => fact_spec,
        _ => return,
    };
    match node_base_tag(&spec) {
        Some(image) => {
            base.name = format!("resolve {image}");
            base.action = Action::ResolveBase { image };
        }
        None => {
            base.name = format!("resolve node {spec} base");
            base.action = Action::ResolveNodeBase { spec, variant: "bookworm-slim".into() };
        }
    }
    plan.steps.retain(|s| s.id != SHIM);
    for s in plan.steps.iter_mut() {
        s.deps.retain(|d| d != SHIM);
    }
    plan.image.layers.retain(|l| l != SHIM);
    if let Some((_, v)) = plan.image.env.iter_mut().find(|(k, _)| k == "PATH") {
        *v = v.trim_end_matches(":/busybox").to_string();
    }
    plan.facts.remove("runtime-base");
    plan.finalize();
}

fn runtime_dev_packages(app: &NodeApp) -> Vec<String> {
    let ts_config = ["next.config.ts", "next.config.mts"].iter().any(|f| app.dir.join(f).exists());
    let next_major = ["dependencies", "devDependencies"]
        .iter()
        .find_map(|k| app.package_json.get(k).and_then(|d| d.get("next")).and_then(|v| v.as_str()))
        .and_then(|spec| spec.trim_start_matches(|c: char| !c.is_ascii_digit()).split('.').next().and_then(|m| m.parse::<u32>().ok()));
    if app.framework == Framework::Next && ts_config && app.has_dep("typescript") && next_major.is_none_or(|m| m < 16) {
        vec!["typescript".into()]
    } else {
        vec![]
    }
}

fn prod_deps_layer(
    b: &mut PlanBuilder,
    app: &NodeApp,
    manager: &str,
    policy: &acropolis_npm::scripts::Policy,
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
        Action::NpmInstall { dev: false, target: "prod".into(), scripts: policy.describe(), manager: manager.into(), types_only: false },
        &[fetch_step, "node"],
    );
    b.step(
        "layer-deps",
        "layer node_modules (production)",
        Action::Layer {
            dest: "app".into(),
            from: LayerFrom::WorkDir { path: "@work/prod".into(), exclude: vec!["package.json".into()] },
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
    fn next_output_detection() {
        let cfg = |t: &str| next_output(&strip_js_comments(t));
        assert_eq!(cfg("const c = { output: 'standalone' }"), Some("standalone".into()));
        assert_eq!(cfg("const c = {\n  // output: \"standalone\",\n  reactStrictMode: true }"), None);
        assert_eq!(cfg("/* output: 'export' */ module.exports = {}"), None);
        assert_eq!(cfg("module.exports = { output: \"export\" }"), Some("export".into()));
        assert_eq!(cfg("export default { output: process.env.X ? 'standalone' : undefined }"), Some(String::new()));
        assert_eq!(cfg("const url = 'http://x.dev/output'; export default {}"), None);
        assert_eq!(cfg("export default { outputFileTracingRoot: x, output:\"export\" }"), Some("export".into()));
        assert_eq!(cfg("export default { outputFileTracingIncludes: {} }"), None);
    }

    #[test]
    fn scripts_run_through_the_shell() {
        assert_eq!(command_argv("./start.sh"), vec!["/bin/sh", "-c", "./start.sh"]);
        assert_eq!(command_argv("bin/server --port 3000")[0], "/bin/sh");
        assert_eq!(command_argv("node server.js"), vec!["node", "server.js"]);
    }

    #[test]
    fn floating_specs() {
        for s in ["22", "lts", "", ">=20", "^24", "^24.0.0", "24.x", "v22", ">=22.0.0"] {
            assert!(floating_node_spec(s), "{s}");
        }
        for s in ["22.2.0", "24.16", "^24.15.0", ">=22.12", "~22.0", "22.11.0"] {
            assert!(!floating_node_spec(s), "{s}");
        }
    }

    #[test]
    fn next_start_ports() {
        assert_eq!(next_start_port("next start"), Some(None));
        assert_eq!(next_start_port("next start -p 4000"), Some(Some("4000".into())));
        assert_eq!(next_start_port("next start --port=8080 -H 0.0.0.0"), Some(Some("8080".into())));
        assert_eq!(next_start_port("next start -p $PORT"), Some(None));
        assert_eq!(next_start_port("node server.js"), None);
        assert_eq!(next_start_port("next start --keepAliveTimeout 5000"), None);
    }

    #[test]
    fn base_tags() {
        assert_eq!(node_base_tag("22").unwrap(), "node:22-bookworm-slim");
        assert_eq!(node_base_tag("v23.5.0").unwrap(), "node:23.5.0-bookworm-slim");
        assert_eq!(node_base_tag("lts").unwrap(), "node:lts-bookworm-slim");
        assert_eq!(node_base_tag(">=18").unwrap(), "node:18-bookworm-slim");
        assert!(node_base_tag("hydrogen").is_none());
    }

    #[test]
    fn argv() {
        assert_eq!(command_argv("node index.js"), vec!["node", "index.js"]);
        assert_eq!(command_argv("NODE_ENV=x node a.js")[0], "/bin/sh");
        assert_eq!(command_argv("node a.js && echo hi")[0], "/bin/sh");
    }
}
