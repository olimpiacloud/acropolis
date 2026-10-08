use crate::hoist::{Graph, PkgId, hoist};
use crate::install::{BinDir, InstallOptions, InstallPackage, InstallPlan, Link, Source, parent_node_modules, relative_link};
use acropolis_store::{Algo, Integrity};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct YarnEntry {
    pub version: String,
    pub resolved: Option<String>,
    pub integrity: Option<String>,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
}

pub struct YarnLock {
    pub entries: HashMap<String, YarnEntry>,
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}

fn split_kv(line: &str) -> (String, String) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        let key = rest[..end].to_string();
        return (key, unquote(&rest[end + 1..]));
    }
    match line.split_once(char::is_whitespace) {
        Some((k, v)) => (k.to_string(), unquote(v)),
        None => (line.trim_end_matches(':').to_string(), String::new()),
    }
}

pub fn name_of_spec(spec: &str) -> (&str, &str) {
    let at = spec[1..].find('@').map(|i| i + 1).unwrap_or(spec.len());
    (&spec[..at], spec.get(at + 1..).unwrap_or(""))
}

impl YarnLock {
    pub fn parse(text: &str) -> Result<Self> {
        if text.contains("__metadata:") {
            bail!("yarn berry lockfiles (v2+) are not supported by the native installer");
        }
        let mut entries: HashMap<String, YarnEntry> = HashMap::new();
        let mut keys: Vec<String> = Vec::new();
        let mut cur = YarnEntry::default();
        let mut section: Option<String> = None;
        let flush = |keys: &mut Vec<String>, cur: &mut YarnEntry, entries: &mut HashMap<String, YarnEntry>| {
            if !keys.is_empty() {
                for k in keys.drain(..) {
                    entries.insert(k, cur.clone());
                }
            }
            *cur = YarnEntry::default();
        };
        for raw in text.lines() {
            if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
                continue;
            }
            let indent = raw.len() - raw.trim_start().len();
            let line = raw.trim_end();
            if indent == 0 {
                flush(&mut keys, &mut cur, &mut entries);
                section = None;
                let header = line.trim_end_matches(':');
                for part in header.split(", ") {
                    keys.push(unquote(part));
                }
            } else if indent == 2 {
                let t = line.trim();
                if t.ends_with(':') && !t.contains(' ') {
                    section = Some(t.trim_end_matches(':').to_string());
                    continue;
                }
                section = None;
                let (k, v) = split_kv(t);
                match k.as_str() {
                    "version" => cur.version = v,
                    "resolved" => cur.resolved = Some(v),
                    "integrity" => cur.integrity = Some(v),
                    _ => {}
                }
            } else if indent >= 4 {
                let (k, v) = split_kv(line.trim());
                match section.as_deref() {
                    Some("dependencies") => {
                        cur.dependencies.insert(k, v);
                    }
                    Some("optionalDependencies") => {
                        cur.optional_dependencies.insert(k, v);
                    }
                    _ => {}
                }
            }
        }
        flush(&mut keys, &mut cur, &mut entries);
        Ok(YarnLock { entries })
    }

    fn lookup(&self, name: &str, range: &str) -> Option<(&String, &YarnEntry)> {
        self.entries.get_key_value(&format!("{name}@{range}"))
    }

    pub fn install_plan(&self, root_pj: &serde_json::Value, workspaces: &[(String, serde_json::Value)], opts: &InstallOptions) -> Result<InstallPlan> {
        let mut graph = Graph::default();
        let mut by_id: HashMap<PkgId, (String, YarnEntry)> = HashMap::new();
        let ws_names: HashMap<String, String> = workspaces
            .iter()
            .filter_map(|(dir, pj)| pj.get("name").and_then(|n| n.as_str()).map(|n| (n.to_string(), dir.clone())))
            .collect();
        let mut root_specs: Vec<(String, String)> = Vec::new();
        let collect = |pj: &serde_json::Value, out: &mut Vec<(String, String)>| {
            let mut kinds = vec!["dependencies"];
            if opts.include_optional {
                kinds.push("optionalDependencies");
            }
            if opts.include_dev {
                kinds.push("devDependencies");
            }
            for k in kinds {
                if let Some(m) = pj.get(k).and_then(|d| d.as_object()) {
                    for (n, r) in m {
                        out.push((n.clone(), r.as_str().unwrap_or("").to_string()));
                    }
                }
            }
        };
        collect(root_pj, &mut root_specs);
        for (_, pj) in workspaces {
            collect(pj, &mut root_specs);
        }
        let mut queue: Vec<(String, String)> = Vec::new();
        for (name, range) in root_specs {
            if ws_names.contains_key(&name) {
                continue;
            }
            match self.lookup(&name, &range) {
                Some((_, e)) => {
                    let id = PkgId(format!("{name}@{}", e.version));
                    graph.roots.entry(name.clone()).or_insert_with(|| id.clone());
                    if !by_id.contains_key(&id) {
                        by_id.insert(id.clone(), (name.clone(), e.clone()));
                        queue.push((name, e.version.clone()));
                    }
                }
                None => bail!("yarn.lock has no entry for {name}@{range}; run `yarn install` to update it"),
            }
        }
        while let Some((name, version)) = queue.pop() {
            let id = PkgId(format!("{name}@{version}"));
            let entry = by_id[&id].1.clone();
            let mut deps = BTreeMap::new();
            let mut all: Vec<(&String, &String)> = entry.dependencies.iter().collect();
            if opts.include_optional {
                all.extend(entry.optional_dependencies.iter());
            }
            for (dn, dr) in all {
                let Some((_, de)) = self.lookup(dn, dr) else {
                    if entry.optional_dependencies.contains_key(dn) {
                        continue;
                    }
                    bail!("yarn.lock has no entry for {dn}@{dr} (needed by {name})");
                };
                let did = PkgId(format!("{dn}@{}", de.version));
                deps.insert(dn.clone(), did.clone());
                if !by_id.contains_key(&did) {
                    by_id.insert(did.clone(), (dn.clone(), de.clone()));
                    queue.push((dn.clone(), de.version.clone()));
                }
            }
            graph.deps.insert(id, deps);
        }
        let layout = hoist(&graph);
        let mut plan = InstallPlan::default();
        let mut bin_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, id) in &layout {
            let (name, e) = &by_id[id];
            if let Some((nm, _)) = parent_node_modules(path) {
                bin_dirs.entry(nm.to_string()).or_default().push(path.clone());
            }
            let resolved = e.resolved.clone().ok_or_else(|| anyhow!("{id:?} has no resolved URL"))?;
            let (url, frag) = match resolved.split_once('#') {
                Some((u, f)) => (u.to_string(), Some(f.to_string())),
                None => (resolved.clone(), None),
            };
            let integrity = match (&e.integrity, &frag) {
                (Some(i), _) => Some(Integrity::parse_sri(i).with_context(|| format!("integrity of {path}"))?),
                (None, Some(f)) if f.len() == 40 => Some(Integrity::parse_hex(Algo::Sha1, f)?),
                _ => None,
            };
            let source = if url.starts_with("http://") || url.starts_with("https://") {
                Source::Registry { url, integrity }
            } else if url.starts_with("git") || url.starts_with("github:") {
                Source::Git { url: resolved.clone() }
            } else {
                bail!("unsupported resolved URL {resolved} for {path}");
            };
            plan.packages.push(InstallPackage {
                path: path.clone(),
                name: name.clone(),
                version: e.version.clone(),
                source,
                bins: None,
                has_install_script: false,
                optional: false,
                dev: false,
            });
        }
        for (name, dir) in &ws_names {
            let path = format!("node_modules/{name}");
            plan.links.push(Link { path: path.clone(), target: relative_link(Path::new(&path), Path::new(dir)) });
        }
        plan.bin_dirs = bin_dirs.into_iter().map(|(dir, packages)| BinDir { dir, packages }).collect();
        Ok(plan)
    }
}

pub fn expand_workspaces(app_dir: &Path, pj: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    let globs: Vec<String> = match pj.get("workspaces") {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
        Some(serde_json::Value::Object(o)) => o
            .get("packages")
            .and_then(|p| p.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    expand_globs(app_dir, &globs)
}

pub fn expand_globs(app_dir: &Path, globs: &[String]) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for g in globs {
        let g = g.trim_start_matches("./").trim_end_matches('/');
        if g.starts_with('!') {
            continue;
        }
        let dirs: Vec<String> = if let Some(base) = g.strip_suffix("/*").or_else(|| g.strip_suffix("/**")) {
            let mut v: Vec<String> = std::fs::read_dir(app_dir.join(base))
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.path().join("package.json").exists())
                        .map(|e| format!("{base}/{}", e.file_name().to_string_lossy()))
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        } else {
            vec![g.to_string()]
        };
        for d in dirs {
            if let Ok(text) = std::fs::read(app_dir.join(&d).join("package.json"))
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&text)
            {
                out.push((d, v));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_v1() {
        let text = "# yarn lockfile v1\n\n\n\"@types/node@^22.13.9\":\n  version \"22.17.1\"\n  resolved \"https://registry.yarnpkg.com/@types/node/-/node-22.17.1.tgz#484a755050497ebc3b37ff5adb7470f2e3ea5f5b\"\n  integrity sha512-y3tBaz+rjspDTylNjAX37jEC3TETEFGNJL6uQDxwF9/8GLLIjW1rvVHlynyuUKMnMr1Roq8jOv3vkopBjC4/VA==\n  dependencies:\n    undici-types \"~6.21.0\"\n\nundici-types@~6.21.0:\n  version \"6.21.0\"\n  resolved \"https://registry.yarnpkg.com/undici-types/-/undici-types-6.21.0.tgz#691d00af3909be93a7faa13be61b3a5b50ef12cb\"\n  integrity sha512-iwDZqg0QAGrg9Rav5H4n0M64c3mkR59cJ6wQp+7C4nI0gsmExaedaYLNO44eT4AtBBwjbTiGPMlt2Md0T9H9JQ==\n";
        let lock = YarnLock::parse(text).unwrap();
        assert_eq!(lock.entries.len(), 2);
        let e = &lock.entries["@types/node@^22.13.9"];
        assert_eq!(e.version, "22.17.1");
        assert_eq!(e.dependencies["undici-types"], "~6.21.0");
        let pj = serde_json::json!({"devDependencies": {"@types/node": "^22.13.9"}});
        let plan = lock
            .install_plan(&pj, &[], &InstallOptions { include_dev: true, include_optional: true, platform: Default::default() })
            .unwrap();
        let paths: Vec<&str> = plan.packages.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["node_modules/@types/node", "node_modules/undici-types"]);
    }
}
