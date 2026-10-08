use crate::install::{InstallPackage, InstallPlan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    All,
    None,
    Only(Vec<String>),
}

impl Policy {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Policy::All => true,
            Policy::None => false,
            Policy::Only(list) => list.iter().any(|n| n == name),
        }
    }

    pub fn parse(s: &str) -> Policy {
        match s.trim() {
            "" | "all" | "true" => Policy::All,
            "none" | "false" | "off" => Policy::None,
            other => Policy::Only(
                other
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect(),
            ),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Policy::All => "all".into(),
            Policy::None => "none".into(),
            Policy::Only(v) => v.join(","),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScriptJob {
    pub path: String,
    pub name: String,
    pub commands: Vec<(String, String)>,
}

pub fn lifecycle_jobs(plan: &InstallPlan, root: &Path, policy: &Policy) -> Vec<ScriptJob> {
    let mut pkgs: Vec<&InstallPackage> = plan.packages.iter().collect();
    pkgs.sort_by(|a, b| {
        let da = a.path.matches("node_modules/").count();
        let db = b.path.matches("node_modules/").count();
        db.cmp(&da).then(a.path.cmp(&b.path))
    });
    let mut out = Vec::new();
    for p in pkgs {
        if !policy.allows(&p.name) {
            continue;
        }
        let dir = root.join(&p.path);
        let Ok(text) = std::fs::read(dir.join("package.json")) else {
            continue;
        };
        let Ok(pj) = serde_json::from_slice::<Value>(&text) else {
            continue;
        };
        let scripts: BTreeMap<String, String> = pj
            .get("scripts")
            .and_then(|s| s.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let mut commands = Vec::new();
        for stage in ["preinstall", "install", "postinstall"] {
            if let Some(c) = scripts.get(stage).filter(|c| !c.trim().is_empty()) {
                commands.push((stage.to_string(), c.clone()));
            }
        }
        if !scripts.contains_key("install") && !scripts.contains_key("preinstall") && dir.join("binding.gyp").exists() {
            commands.insert(0, ("install".to_string(), "node-gyp rebuild".to_string()));
        }
        if !commands.is_empty() {
            out.push(ScriptJob {
                path: p.path.clone(),
                name: p.name.clone(),
                commands,
            });
        }
    }
    out
}

pub fn policy_for(manager: &str, package_json: &Value, override_: Option<&str>) -> Policy {
    if let Some(o) = override_ {
        return Policy::parse(o);
    }
    match manager {
        "pnpm" => {
            let only = package_json
                .get("pnpm")
                .and_then(|p| p.get("onlyBuiltDependencies"))
                .and_then(|o| o.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                });
            match only {
                Some(list) => Policy::Only(list),
                None => Policy::All,
            }
        }
        "bun" => {
            let mut list: Vec<String> = package_json
                .get("trustedDependencies")
                .and_then(|o| o.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            list.extend(BUN_DEFAULT_TRUSTED.iter().map(|s| s.to_string()));
            Policy::Only(list)
        }
        _ => Policy::All,
    }
}

pub const BUN_DEFAULT_TRUSTED: &[&str] = &[
    "@biomejs/biome",
    "@prisma/client",
    "@prisma/engines",
    "@swc/core",
    "bcrypt",
    "better-sqlite3",
    "canvas",
    "esbuild",
    "node-sass",
    "prisma",
    "puppeteer",
    "sharp",
    "sqlite3",
];
