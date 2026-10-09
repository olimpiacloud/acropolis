use crate::install::{InstallPackage, InstallPlan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    All,
    None,
    Only(Vec<String>),
    /// Every package except these (`!a,!b`).
    Except(Vec<String>),
}

impl Policy {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Policy::All => true,
            Policy::None => false,
            Policy::Only(list) => list.iter().any(|n| n == name),
            Policy::Except(list) => !list.iter().any(|n| n == name),
        }
    }

    pub fn parse(s: &str) -> Policy {
        match s.trim() {
            "" | "all" | "true" => Policy::All,
            "none" | "false" | "off" => Policy::None,
            other => {
                let names = other.split(',').map(|x| x.trim()).filter(|x| !x.is_empty());
                if other.starts_with('!') {
                    Policy::Except(names.map(|x| x.trim_start_matches('!').to_string()).collect())
                } else {
                    Policy::Only(names.map(|x| x.to_string()).collect())
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Policy::All => "all".into(),
            Policy::None => "none".into(),
            Policy::Only(v) => v.join(","),
            Policy::Except(v) => v.iter().map(|n| format!("!{n}")).collect::<Vec<_>>().join(","),
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

pub fn policy_for(manager: &str, app_root: &Path, package_json: &Value, override_: Option<&str>) -> Policy {
    if let Some(o) = override_ {
        return Policy::parse(o);
    }
    match manager {
        "pnpm" => pnpm_policy(app_root, package_json),
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

/// pnpm's dependency build rules, from `package.json#pnpm` or `pnpm-workspace.yaml`. Without an allow or deny
/// list pnpm >= 10 builds nothing and older versions build everything. The version comes from `packageManager`
/// only: lockfile 9.0 is written by pnpm 9, 10 and 11, and Railpack installs pnpm 9 for it.
fn pnpm_policy(app_root: &Path, package_json: &Value) -> Policy {
    let workspace: Value = std::fs::read_to_string(app_root.join("pnpm-workspace.yaml"))
        .ok()
        .and_then(|t| serde_yaml::from_str(&t).ok())
        .unwrap_or_default();
    let setting = |key: &str| {
        package_json
            .get("pnpm")
            .and_then(|p| p.get(key))
            .or_else(|| workspace.get(key))
    };
    let names = |key: &str| {
        setting(key).and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
    };
    if setting("dangerouslyAllowAllBuilds").and_then(|v| v.as_bool()) == Some(true) {
        return Policy::All;
    }
    if let Some(only) = names("onlyBuiltDependencies") {
        return Policy::Only(only);
    }
    if let Some(mut never) = names("neverBuiltDependencies") {
        never.extend(names("ignoredBuiltDependencies").unwrap_or_default());
        return if never.is_empty() {
            Policy::All
        } else {
            Policy::Except(never)
        };
    }
    let major = package_json
        .get("packageManager")
        .and_then(|v| v.as_str())
        .and_then(|s| s.strip_prefix("pnpm@"))
        .and_then(|v| v.split('.').next()?.parse::<u64>().ok());
    if major.is_some_and(|m| m >= 10) {
        Policy::None
    } else {
        Policy::All
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pnpm_build_policy_follows_version_and_lists() {
        let dir = std::env::temp_dir().join(format!("acropolis-pnpm-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = |pj: Value| policy_for("pnpm", &dir, &pj, None);
        assert_eq!(policy(json!({})), Policy::All);
        assert_eq!(policy(json!({"packageManager": "pnpm@9.15.0"})), Policy::All);
        assert_eq!(
            policy(json!({"packageManager": "pnpm@10.4.1+sha256.abc"})),
            Policy::None
        );
        assert_eq!(
            policy(json!({"packageManager": "pnpm@10.4.1", "pnpm": {"onlyBuiltDependencies": ["esbuild"]}})),
            Policy::Only(vec!["esbuild".into()])
        );
        assert_eq!(
            policy(json!({"packageManager": "pnpm@10.4.1", "pnpm": {"neverBuiltDependencies": []}})),
            Policy::All
        );
        let never =
            policy(json!({"pnpm": {"neverBuiltDependencies": ["fsevents"], "ignoredBuiltDependencies": ["sharp"]}}));
        assert!(never.allows("esbuild") && !never.allows("fsevents") && !never.allows("sharp"));
        assert_eq!(Policy::parse(&never.describe()), never);
        std::fs::write(dir.join("pnpm-workspace.yaml"), "dangerouslyAllowAllBuilds: true\n").unwrap();
        assert_eq!(policy(json!({"packageManager": "pnpm@10.4.1"})), Policy::All);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
