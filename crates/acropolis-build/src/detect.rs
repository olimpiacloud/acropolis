use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub vars: BTreeMap<String, String>,
}

impl Env {
    pub fn config(&self, name: &str) -> Option<(String, String)> {
        for key in [format!("ACROPOLIS_{name}"), format!("RAILPACK_{name}")] {
            if let Some(v) = self.vars.get(&key).filter(|v| !v.trim().is_empty()) {
                return Some((v.trim().to_string(), key));
            }
        }
        None
    }

    pub fn flag(&self, name: &str) -> bool {
        self.config(name).map(|(v, _)| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct VersionSpec {
    pub spec: String,
    pub source: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn1,
    YarnBerry,
    Bun,
}

impl PackageManager {
    pub fn name(self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn1 => "yarn",
            PackageManager::YarnBerry => "yarn",
            PackageManager::Bun => "bun",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Framework {
    None,
    Next,
    Vite,
    TanstackStart,
    Astro,
    Remix,
    ReactRouter,
    Nuxt,
    SvelteKit,
    Cra,
    Angular,
}

#[derive(Clone, Debug)]
pub struct NodeApp {
    pub dir: PathBuf,
    pub package_json: Value,
    pub pm: PackageManager,
    pub pm_version: Option<String>,
    pub lockfile: Option<String>,
    pub node: VersionSpec,
    pub scripts: BTreeMap<String, String>,
    pub framework: Framework,
    pub has_workspaces: bool,
    pub root: PathBuf,
    pub member: Option<String>,
}

impl NodeApp {
    pub fn script(&self, name: &str) -> Option<&str> {
        self.scripts.get(name).map(|s| s.as_str()).filter(|s| !s.trim().is_empty())
    }

    pub fn has_dep(&self, name: &str) -> bool {
        ["dependencies", "devDependencies", "optionalDependencies"]
            .iter()
            .any(|k| self.package_json.get(k).and_then(|d| d.get(name)).is_some())
    }

    pub fn has_prod_dep(&self, name: &str) -> bool {
        self.package_json.get("dependencies").and_then(|d| d.get(name)).is_some()
    }

    pub fn main(&self) -> Option<String> {
        self.package_json.get("main").and_then(|m| m.as_str()).map(|s| s.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct GoApp {
    pub dir: PathBuf,
    pub module: String,
    pub go: VersionSpec,
    pub package: String,
    pub cgo: bool,
    pub has_sum: bool,
}

#[derive(Clone, Debug)]
pub enum App {
    Node(NodeApp),
    Go(GoApp),
    Rust(crate::providers::rust::RustApp),
}

impl App {
    pub fn provider(&self) -> &'static str {
        match self {
            App::Node(_) => "node",
            App::Go(_) => "go",
            App::Rust(_) => "rust",
        }
    }
}

pub fn read_json_lenient(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str(&text) {
        Ok(v) => Ok(v),
        Err(_) => json5::from_str(&text).with_context(|| format!("parsing {}", path.display())),
    }
}

fn read_trim(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.lines().next().unwrap_or("").trim().to_string()).filter(|s| !s.is_empty())
}

fn mise_locked(dir: &Path, tool: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("mise.lock")).ok()?;
    let header = format!("[[tools.{tool}]]");
    let mut in_tool = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_tool = l == header;
            continue;
        }
        if in_tool
            && let Some(v) = l.strip_prefix("version")
            && let Some(v) = v.trim_start().strip_prefix('=')
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

pub fn tool_version(dir: &Path, tool: &str) -> Option<VersionSpec> {
    for file in ["mise.toml", ".mise.toml", "mise/config.toml", ".config/mise.toml"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            let mut in_tools = false;
            for line in text.lines() {
                let l = line.trim();
                if l.starts_with('[') {
                    in_tools = l == "[tools]";
                    continue;
                }
                if !in_tools {
                    continue;
                }
                if let Some((k, v)) = l.split_once('=') {
                    let k = k.trim().trim_matches('"');
                    if k == tool {
                        let v = v.trim();
                        let v = if v.starts_with('{') {
                            v.split("version").nth(1).and_then(|r| r.split('"').nth(1)).unwrap_or("").to_string()
                        } else if v.starts_with('[') {
                            v.split('"').nth(1).unwrap_or("").to_string()
                        } else {
                            v.trim_matches('"').trim_matches('\'').to_string()
                        };
                        if v == "latest"
                            && let Some(locked) = mise_locked(dir, tool)
                        {
                            return Some(VersionSpec { spec: locked, source: "mise.lock".into() });
                        }
                        if !v.is_empty() {
                            return Some(VersionSpec { spec: v, source: file.to_string() });
                        }
                    }
                }
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join(".tool-versions")) {
        let names: Vec<&str> = match tool {
            "node" => vec!["node", "nodejs"],
            "go" => vec!["go", "golang"],
            other => vec![other],
        };
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next())
                && names.contains(&k)
            {
                return Some(VersionSpec { spec: v.to_string(), source: ".tool-versions".into() });
            }
        }
    }
    None
}

pub fn detect(dir: &Path, env: &Env) -> Result<App> {
    if has_package_json(dir) {
        return Ok(App::Node(detect_node(dir, env)?));
    }
    if dir.join("go.mod").exists() || dir.join("go.work").exists() || dir.join("main.go").exists() {
        return Ok(App::Go(detect_go(dir, env)?));
    }
    if dir.join("Cargo.toml").exists() {
        return Ok(App::Rust(crate::providers::rust::detect(dir, env)?));
    }
    bail!("could not detect how to build {}: no package.json or go.mod", dir.display())
}

pub fn has_package_json(dir: &Path) -> bool {
    dir.join("package.json").exists() || dir.join("package.json5").exists() || dir.join("package.yaml").exists()
}

pub fn read_package_json(dir: &Path) -> Result<Value> {
    if dir.join("package.json").exists() {
        return read_json_lenient(&dir.join("package.json"));
    }
    if dir.join("package.json5").exists() {
        return read_json_lenient(&dir.join("package.json5"));
    }
    let text = std::fs::read_to_string(dir.join("package.yaml")).context("reading package.yaml")?;
    serde_yaml::from_str(&text).context("parsing package.yaml")
}

pub fn workspace_members(root: &Path, pj: &Value) -> Vec<String> {
    let mut globs: Vec<String> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("pnpm-workspace.yaml")) {
        let mut in_packages = false;
        for line in text.lines() {
            if !line.starts_with(' ') && !line.starts_with('-') {
                in_packages = line.trim_end() == "packages:";
                continue;
            }
            if in_packages && let Some(g) = line.trim().strip_prefix('-') {
                globs.push(g.trim().trim_matches(['"', '\'']).to_string());
            }
        }
    }
    let ws = pj.get("workspaces");
    let arr = ws.and_then(|w| w.as_array()).or_else(|| ws.and_then(|w| w.get("packages")).and_then(|p| p.as_array()));
    for g in arr.into_iter().flatten().filter_map(|g| g.as_str()) {
        globs.push(g.to_string());
    }
    let mut out = Vec::new();
    for g in globs {
        let g = g.trim_start_matches("./").trim_end_matches('/');
        if g.starts_with('!') {
            continue;
        }
        if let Some(parent) = g.strip_suffix("/*").or_else(|| g.strip_suffix("/**")) {
            let Ok(rd) = std::fs::read_dir(root.join(parent)) else { continue };
            for e in rd.flatten() {
                if e.path().join("package.json").exists() {
                    out.push(format!("{parent}/{}", e.file_name().to_string_lossy()));
                }
            }
        } else if root.join(g).join("package.json").exists() {
            out.push(g.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

const LOCKFILES: &[(&str, PackageManager)] = &[
    ("package-lock.json", PackageManager::Npm),
    ("pnpm-lock.yaml", PackageManager::Pnpm),
    ("bun.lock", PackageManager::Bun),
    ("bun.lockb", PackageManager::Bun),
    ("yarn.lock", PackageManager::Yarn1),
];

pub fn workspace_root(dir: &Path) -> Option<(PathBuf, String)> {
    let dir = std::fs::canonicalize(dir).ok()?;
    if LOCKFILES.iter().any(|(f, _)| dir.join(f).exists()) {
        return None;
    }
    let mut cur = dir.parent();
    for _ in 0..6 {
        let root = cur?;
        if has_package_json(root) || root.join("pnpm-workspace.yaml").exists() {
            let pj = read_package_json(root).unwrap_or(Value::Null);
            let rel = dir.strip_prefix(root).ok()?.to_string_lossy().into_owned();
            if workspace_members(root, &pj).contains(&rel) {
                return Some((root.to_path_buf(), rel));
            }
        }
        cur = root.parent();
    }
    None
}

pub fn detect_node(dir: &Path, env: &Env) -> Result<NodeApp> {
    let member = workspace_root(dir);
    let root = member.as_ref().map(|(r, _)| r.clone()).unwrap_or_else(|| dir.to_path_buf());
    let root_pj = if member.is_some() { read_package_json(&root)? } else { Value::Null };
    let pj = read_package_json(dir)?;
    let scripts: BTreeMap<String, String> = pj
        .get("scripts")
        .and_then(|s| s.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default();
    let dev_engine = pj.get("devEngines").and_then(|d| d.get("packageManager")).and_then(|p| {
        let first = if let Some(a) = p.as_array() { a.first()? } else { p };
        let name = first.get("name")?.as_str()?;
        let version = first.get("version").and_then(|v| v.as_str()).unwrap_or("");
        Some(if version.is_empty() { name.to_string() } else { format!("{name}@{version}") })
    });
    let root_dev_engine = root_pj.get("devEngines").and_then(|d| d.get("packageManager")).and_then(|p| {
        let first = if let Some(a) = p.as_array() { a.first()? } else { p };
        let name = first.get("name")?.as_str()?;
        let version = first.get("version").and_then(|v| v.as_str()).unwrap_or("");
        Some(if version.is_empty() { name.to_string() } else { format!("{name}@{version}") })
    });
    let pm_field = root_pj
        .get("packageManager")
        .and_then(|p| p.as_str())
        .map(|s| s.to_string())
        .or(root_dev_engine)
        .or_else(|| pj.get("packageManager").and_then(|p| p.as_str()).map(|s| s.to_string()))
        .or(dev_engine);
    let (mut pm, mut lockfile) = (PackageManager::Npm, None);
    for (file, m) in LOCKFILES {
        if root.join(file).exists() {
            pm = *m;
            lockfile = Some(file.to_string());
            break;
        }
    }
    if lockfile.is_none() && pm_field.is_none() {
        let eng = |k: &str| pj.get("engines").and_then(|e| e.get(k)).and_then(|v| v.as_str()).map(|s| s.to_string());
        if eng("pnpm").is_some() || dir.join("pnpm-workspace.yaml").exists() || dir.join("package.json5").exists() || dir.join("package.yaml").exists() {
            pm = PackageManager::Pnpm;
        } else if eng("bun").is_some() {
            pm = PackageManager::Bun;
        }
    }
    let mut pm_version = None;
    if let Some(f) = &pm_field {
        let (name, ver) = f.split_once('@').unwrap_or((f.as_str(), ""));
        let ver = ver.split('+').next().unwrap_or("").to_string();
        pm = match name {
            "pnpm" => PackageManager::Pnpm,
            "yarn" => {
                if ver.starts_with('1') || ver.is_empty() {
                    PackageManager::Yarn1
                } else {
                    PackageManager::YarnBerry
                }
            }
            "bun" => PackageManager::Bun,
            _ => PackageManager::Npm,
        };
        if !ver.is_empty() {
            pm_version = Some(ver);
        }
    }
    if pm == PackageManager::Yarn1 && pm_field.is_none() {
        if let Ok(text) = std::fs::read_to_string(root.join("yarn.lock"))
            && text.contains("__metadata:")
        {
            pm = PackageManager::YarnBerry;
        }
    }
    let mut node = node_version(dir, &pj, env);
    if member.is_some() && node.source == "default" {
        node = node_version(&root, &root_pj, env);
    }
    let framework = detect_framework(&pj, &scripts);
    let has_workspaces = member.is_none() && (pj.get("workspaces").is_some() || dir.join("pnpm-workspace.yaml").exists());
    Ok(NodeApp {
        dir: dir.to_path_buf(),
        package_json: pj,
        pm,
        pm_version,
        lockfile,
        node,
        scripts,
        framework,
        has_workspaces,
        root,
        member: member.map(|(_, rel)| rel),
    })
}

fn node_version(dir: &Path, pj: &Value, env: &Env) -> VersionSpec {
    if let Some(v) = tool_version(dir, "node") {
        return v;
    }
    for f in [".nvmrc", ".node-version"] {
        if let Some(v) = read_trim(&dir.join(f)) {
            return VersionSpec { spec: v.trim_start_matches('v').to_string(), source: f.into() };
        }
    }
    if let Some((v, k)) = env.config("NODE_VERSION") {
        return VersionSpec { spec: v, source: k };
    }
    if let Some(rt) = pj.get("devEngines").and_then(|d| d.get("runtime")) {
        let items: Vec<&Value> = if let Some(a) = rt.as_array() { a.iter().collect() } else { vec![rt] };
        for it in items {
            if it.get("name").and_then(|n| n.as_str()) == Some("node")
                && let Some(v) = it.get("version").and_then(|v| v.as_str())
            {
                return VersionSpec { spec: v.to_string(), source: "package.json devEngines.runtime".into() };
            }
        }
    }
    if let Some(e) = pj.get("engines").and_then(|e| e.get("node")).and_then(|v| v.as_str())
        && !e.trim().is_empty()
    {
        return VersionSpec { spec: e.trim().to_string(), source: "package.json engines.node".into() };
    }
    VersionSpec { spec: "lts".into(), source: "default".into() }
}

fn detect_framework(pj: &Value, scripts: &BTreeMap<String, String>) -> Framework {
    let has = |name: &str| {
        ["dependencies", "devDependencies"].iter().any(|k| pj.get(k).and_then(|d| d.get(name)).is_some())
    };
    let prod = |name: &str| pj.get("dependencies").and_then(|d| d.get(name)).is_some();
    let build = scripts.get("build").map(|s| s.as_str()).unwrap_or("");
    if has("next") && (build.contains("next") || build.is_empty()) {
        return Framework::Next;
    }
    if prod("@tanstack/react-start") || prod("@tanstack/solid-start") {
        return Framework::TanstackStart;
    }
    if has("nuxt") {
        return Framework::Nuxt;
    }
    if has("@sveltejs/kit") {
        return Framework::SvelteKit;
    }
    if has("astro") {
        return Framework::Astro;
    }
    if has("@remix-run/react") || has("@remix-run/dev") {
        return Framework::Remix;
    }
    if has("@react-router/dev") {
        return Framework::ReactRouter;
    }
    if has("@angular/core") {
        return Framework::Angular;
    }
    if has("react-scripts") {
        return Framework::Cra;
    }
    if has("vite") {
        return Framework::Vite;
    }
    Framework::None
}

pub fn detect_go(dir: &Path, env: &Env) -> Result<GoApp> {
    let gomod = if dir.join("go.mod").exists() { Some(acropolis_gomod::read_go_mod(dir)?) } else { None };
    let go = if let Some((v, k)) = env.config("GO_VERSION") {
        VersionSpec { spec: v, source: k }
    } else if let Some(v) = tool_version(dir, "go") {
        v
    } else if let Some(t) = gomod.as_ref().and_then(|m| m.toolchain.clone()) {
        VersionSpec { spec: t, source: "go.mod toolchain".into() }
    } else if let Some(g) = gomod.as_ref().and_then(|m| m.go.clone()) {
        VersionSpec { spec: g, source: "go.mod go".into() }
    } else if let Some(g) = std::fs::read_to_string(dir.join("go.work"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.trim().strip_prefix("go ").map(|v| v.trim().to_string())))
    {
        VersionSpec { spec: g, source: "go.work".into() }
    } else {
        VersionSpec { spec: acropolis_toolchain::go::DEFAULT_GO.into(), source: "default".into() }
    };
    let package = if let Some((p, _)) = env.config("GO_BIN") {
        format!("./cmd/{p}")
    } else if has_go_files(dir) {
        ".".to_string()
    } else if let Some(cmd) = first_dir(&dir.join("cmd")) {
        format!("./cmd/{cmd}")
    } else if let Some(m) = workspace_main(dir) {
        format!("./{m}")
    } else {
        ".".to_string()
    };
    let cgo = env.vars.get("CGO_ENABLED").map(|v| v == "1").unwrap_or(false);
    Ok(GoApp {
        dir: dir.to_path_buf(),
        module: gomod.map(|m| m.module).unwrap_or_default(),
        go,
        package,
        cgo,
        has_sum: dir.join("go.sum").exists(),
    })
}

fn workspace_main(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("go.work")).ok()?;
    let mut uses = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let l = line.split("//").next().unwrap_or("").trim();
        if l.starts_with("use (") {
            in_block = true;
            continue;
        }
        if in_block {
            if l == ")" {
                in_block = false;
                continue;
            }
            if !l.is_empty() {
                uses.push(l.trim_start_matches("./").to_string());
            }
        } else if let Some(u) = l.strip_prefix("use ") {
            uses.push(u.trim().trim_start_matches("./").to_string());
        }
    }
    uses.into_iter().find(|u| {
        std::fs::read_dir(dir.join(u))
            .map(|rd| {
                rd.flatten().any(|e| {
                    e.file_name().to_string_lossy().ends_with(".go")
                        && std::fs::read_to_string(e.path()).map(|t| t.contains("package main")).unwrap_or(false)
                })
            })
            .unwrap_or(false)
    })
}

fn has_go_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.file_name().to_string_lossy().ends_with(".go")))
        .unwrap_or(false)
}

fn first_dir(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names.into_iter().next()
}
