use crate::install::{
    BinDir, InstallOptions, InstallPackage, InstallPlan, Link, Source, default_registry_url, platform_matches_named,
    relative_link,
};
use crate::lockfile::bins_of;
use acropolis_store::Integrity;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    spec: String,
    registry: String,
    meta: Value,
    integrity: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct Workspace {
    path: String,
    name: String,
    deps: BTreeMap<String, String>,
    dev: BTreeMap<String, String>,
    optional: BTreeMap<String, String>,
}

pub struct BunLock {
    workspaces: Vec<Workspace>,
    entries: BTreeMap<String, Entry>,
}

fn obj_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(|v| v.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
        .unwrap_or_default()
}

fn str_or_list(v: Option<&Value>) -> Option<Vec<String>> {
    match v? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()),
        _ => None,
    }
}

fn split_spec(spec: &str) -> (String, String) {
    let at = spec[1..].find('@').map(|i| i + 1).unwrap_or(spec.len());
    (spec[..at].to_string(), spec.get(at + 1..).unwrap_or("").to_string())
}

pub fn key_to_path(key: &str) -> String {
    let parts: Vec<&str> = key.split('/').collect();
    let mut names: Vec<String> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        if parts[i].starts_with('@') && i + 1 < parts.len() {
            names.push(format!("{}/{}", parts[i], parts[i + 1]));
            i += 2;
        } else {
            names.push(parts[i].to_string());
            i += 1;
        }
    }
    names.iter().map(|n| format!("node_modules/{n}")).collect::<Vec<_>>().join("/")
}

fn key_segments(key: &str) -> Vec<String> {
    let parts: Vec<&str> = key.split('/').collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        if parts[i].starts_with('@') && i + 1 < parts.len() {
            names.push(format!("{}/{}", parts[i], parts[i + 1]));
            i += 2;
        } else {
            names.push(parts[i].to_string());
            i += 1;
        }
    }
    names
}

impl BunLock {
    pub fn parse(text: &str) -> Result<Self> {
        let root: Value = json5::from_str(text).context("parsing bun.lock")?;
        let mut workspaces = Vec::new();
        if let Some(ws) = root.get("workspaces").and_then(|w| w.as_object()) {
            for (path, w) in ws {
                workspaces.push(Workspace {
                    path: path.clone(),
                    name: w.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                    deps: obj_map(w.get("dependencies")),
                    dev: obj_map(w.get("devDependencies")),
                    optional: obj_map(w.get("optionalDependencies")),
                });
            }
        }
        let mut entries = BTreeMap::new();
        if let Some(pk) = root.get("packages").and_then(|p| p.as_object()) {
            for (key, v) in pk {
                let arr = v.as_array().cloned().unwrap_or_default();
                let spec = arr.first().and_then(|s| s.as_str()).unwrap_or("").to_string();
                let (name, rest) = split_spec(&spec);
                let (registry, meta, integrity) = if rest.starts_with("workspace:")
                    || rest.starts_with("link:")
                    || rest.starts_with("file:")
                {
                    (String::new(), arr.get(1).cloned().unwrap_or(Value::Null), None)
                } else if arr.get(1).map(|v| v.is_string()).unwrap_or(false) {
                    (
                        arr[1].as_str().unwrap_or("").to_string(),
                        arr.get(2).cloned().unwrap_or(Value::Null),
                        arr.get(3).and_then(|i| i.as_str()).map(|s| s.to_string()),
                    )
                } else {
                    (String::new(), arr.get(1).cloned().unwrap_or(Value::Null), arr.get(2).and_then(|i| i.as_str()).map(|s| s.to_string()))
                };
                entries.insert(key.clone(), Entry { name, spec: rest, registry, meta, integrity });
            }
        }
        Ok(BunLock { workspaces, entries })
    }

    fn resolve(&self, from_key: &str, dep: &str) -> Option<String> {
        let segs = key_segments(from_key);
        for depth in (0..=segs.len()).rev() {
            let prefix = segs[..depth].join("/");
            let cand = if prefix.is_empty() { dep.to_string() } else { format!("{prefix}/{dep}") };
            if self.entries.contains_key(&cand) {
                return Some(cand);
            }
        }
        None
    }

    pub fn install_plan(&self, opts: &InstallOptions) -> Result<InstallPlan> {
        self.install_plan_scoped(opts, &[])
    }

    pub fn install_plan_scoped(&self, opts: &InstallOptions, only: &[String]) -> Result<InstallPlan> {
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut plan = InstallPlan::default();
        for w in self.workspaces.iter().filter(|w| only.is_empty() || only.contains(&w.path)) {
            let scope = if w.path.is_empty() { String::new() } else { w.name.clone() };
            let mut roots: Vec<&String> = w.deps.keys().collect();
            if opts.include_optional {
                roots.extend(w.optional.keys());
            }
            if opts.include_dev {
                roots.extend(w.dev.keys());
            }
            for d in roots {
                if let Some(k) = self.resolve(&scope, d) {
                    queue.push_back(k);
                }
            }
        }
        while let Some(k) = queue.pop_front() {
            if reachable.contains(&k) {
                continue;
            }
            let e = &self.entries[&k];
            let os = str_or_list(e.meta.get("os"));
            let cpu = str_or_list(e.meta.get("cpu"));
            let libc = str_or_list(e.meta.get("libc"));
            if !platform_matches_named(&e.name, &os, &cpu, &libc, &opts.platform) {
                plan.skipped_platform.push(k.clone());
                continue;
            }
            reachable.insert(k.clone());
            let mut deps: Vec<String> = obj_map(e.meta.get("dependencies")).into_keys().collect();
            deps.extend(obj_map(e.meta.get("peerDependencies")).into_keys().filter(|p| {
                let optional = e.meta.get("peerDependenciesMeta").and_then(|m| m.get(p)).and_then(|m| m.get("optional")).and_then(|o| o.as_bool()).unwrap_or(false);
                !optional
            }));
            if opts.include_optional {
                deps.extend(obj_map(e.meta.get("optionalDependencies")).into_keys());
            }
            if let Some(ws) = e.spec.strip_prefix("workspace:")
                && let Some(w) = self.workspaces.iter().find(|w| w.path == ws)
            {
                deps.extend(w.deps.keys().cloned());
                if opts.include_optional {
                    deps.extend(w.optional.keys().cloned());
                }
            }
            for d in deps {
                if let Some(c) = self.resolve(&k, &d) {
                    queue.push_back(c);
                }
            }
        }
        let mut bin_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for k in &reachable {
            let e = &self.entries[k];
            let path = key_to_path(k);
            let nm = path.rsplit_once("/node_modules/").map(|(a, _)| format!("{a}/node_modules")).unwrap_or_else(|| "node_modules".into());
            bin_dirs.entry(nm).or_default().push(path.clone());
            let bins = bins_of(&e.name, &e.meta.get("bin").cloned());
            if let Some(target) = e.spec.strip_prefix("workspace:").or_else(|| e.spec.strip_prefix("link:")).or_else(|| e.spec.strip_prefix("file:")) {
                let target = target.trim_start_matches("./");
                plan.links.push(Link { path: path.clone(), target: relative_link(Path::new(&path), Path::new(target)) });
                if !bins.is_empty() {
                    plan.known_bins.insert(path.clone(), bins);
                }
                continue;
            }
            let source = if e.spec.starts_with("github:") || e.spec.starts_with("git+") || e.spec.starts_with("git:") {
                Source::Git { url: e.spec.clone() }
            } else if e.spec.starts_with("http://") || e.spec.starts_with("https://") {
                Source::Registry { url: e.spec.clone(), integrity: None }
            } else {
                let url = if e.registry.is_empty() {
                    default_registry_url(&e.name, &e.spec)
                } else if e.registry.ends_with(".tgz") {
                    e.registry.clone()
                } else {
                    let short = e.name.rsplit('/').next().unwrap_or(&e.name);
                    format!("{}/{}/-/{}-{}.tgz", e.registry.trim_end_matches('/'), e.name, short, e.spec)
                };
                let integrity = match &e.integrity {
                    Some(i) => Some(Integrity::parse_sri(i).with_context(|| format!("integrity of {k}"))?),
                    None => None,
                };
                Source::Registry { url, integrity }
            };
            plan.packages.push(InstallPackage {
                path,
                name: e.name.clone(),
                version: e.spec.clone(),
                source,
                bins: Some(bins),
                has_install_script: false,
                optional: false,
                dev: false,
            });
        }
        plan.bin_dirs = bin_dirs.into_iter().map(|(dir, packages)| BinDir { dir, packages }).collect();
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(key_to_path("body-parser/debug"), "node_modules/body-parser/node_modules/debug");
        assert_eq!(key_to_path("@babel/core/semver"), "node_modules/@babel/core/node_modules/semver");
        assert_eq!(key_to_path("@a/b"), "node_modules/@a/b");
    }

    #[test]
    fn workspace_lock() {
        let text = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root", "dependencies": { "pkg-b": "workspace:*", }, },
    "packages/pkg-a": { "name": "pkg-a", "dependencies": { "abbrev": "^3.0.0", }, },
    "packages/pkg-b": { "name": "pkg-b", "dependencies": { "pkg-a": "workspace:*", }, },
  },
  "packages": {
    "abbrev": ["abbrev@3.0.1", "", {}, "sha512-AO2ac6pjRB3SJmGJo+v5/aK6Omggp6fsLrs6wN9bd35ulu4cCwaAU9+7ZhXjeqHVkaHThLuzH0nZr0YpCDhygg=="],
    "pkg-a": ["pkg-a@workspace:packages/pkg-a"],
    "pkg-b": ["pkg-b@workspace:packages/pkg-b"],
  }
}"#;
        let lock = BunLock::parse(text).unwrap();
        let plan = lock
            .install_plan(&InstallOptions { include_dev: false, include_optional: true, platform: Default::default() })
            .unwrap();
        assert_eq!(plan.packages.len(), 1);
        assert_eq!(plan.links.len(), 2);
        let b = plan.links.iter().find(|l| l.path == "node_modules/pkg-b").unwrap();
        assert_eq!(b.target, "../packages/pkg-b");
        let scoped = lock
            .install_plan_scoped(&InstallOptions { include_dev: false, include_optional: true, platform: Default::default() }, &["packages/pkg-a".into()])
            .unwrap();
        assert_eq!(scoped.packages.len(), 1);
        assert!(scoped.links.is_empty());
        let scoped = lock
            .install_plan_scoped(&InstallOptions { include_dev: false, include_optional: true, platform: Default::default() }, &["packages/pkg-b".into()])
            .unwrap();
        assert_eq!(scoped.links.len(), 1);
        assert_eq!(scoped.packages.len(), 1);
    }
}
