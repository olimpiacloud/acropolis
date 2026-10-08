use crate::hoist::{Graph, PkgId, hoist};
use crate::install::{BinDir, InstallOptions, InstallPackage, InstallPlan, Link, Source, parent_node_modules, platform_matches, relative_link};
use crate::lockfile::bins_of;
use acropolis_fetch::Fetcher;
use acropolis_store::Integrity;
use anyhow::{Context, Result, anyhow, bail};
use futures::future::join_all;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use semver::Version;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const REGISTRY: &str = "https://registry.npmjs.org";

fn escape_name(name: &str) -> String {
    name.replace('/', "%2f")
}

fn pick_version(packument: &Value, range: &str) -> Option<String> {
    let range = range.trim();
    let tags = packument.get("dist-tags");
    if let Some(v) = tags.and_then(|t| t.get(range)).and_then(|v| v.as_str()) {
        return Some(v.to_string());
    }
    let versions: Vec<Version> = packument
        .get("versions")
        .and_then(|v| v.as_object())
        .map(|m| m.keys().filter_map(|k| Version::parse(k).ok()).collect())
        .unwrap_or_default();
    let reqs = acropolis_semver::parse_range(if range.is_empty() { "*" } else { range });
    if let Some(latest) = tags.and_then(|t| t.get("latest")).and_then(|v| v.as_str())
        && let Ok(lv) = Version::parse(latest)
        && reqs.iter().any(|r| r.matches(&lv))
    {
        return Some(latest.to_string());
    }
    let mut best: Option<&Version> = None;
    for v in &versions {
        let allowed = v.pre.is_empty() || reqs.iter().any(|r| r.matches(v) && r.comparators.iter().any(|c| !c.pre.is_empty()));
        if allowed && reqs.iter().any(|r| r.matches(v)) && best.map(|b| v > b).unwrap_or(true) {
            best = Some(v);
        }
    }
    best.map(|v| v.to_string())
}

pub struct Resolver<'a> {
    fetcher: &'a Fetcher,
    registry: String,
    packuments: HashMap<String, Value>,
}

impl<'a> Resolver<'a> {
    pub fn new(fetcher: &'a Fetcher, registry: Option<&str>) -> Self {
        Resolver { fetcher, registry: registry.unwrap_or(REGISTRY).trim_end_matches('/').to_string(), packuments: HashMap::new() }
    }

    async fn load(&mut self, names: Vec<String>) -> Result<()> {
        let missing: Vec<String> = names.into_iter().filter(|n| !self.packuments.contains_key(n)).collect::<HashSet<_>>().into_iter().collect();
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8"));
        let fetcher = self.fetcher;
        let registry = self.registry.clone();
        let futs = missing.iter().map(|n| {
            let url = format!("{}/{}", registry, escape_name(n));
            let headers = headers.clone();
            async move {
                let b = fetcher.bytes_with(&url, &headers).await.with_context(|| format!("fetching metadata for {n}"))?;
                let v: Value = serde_json::from_slice(&b).with_context(|| format!("parsing metadata for {n}"))?;
                Ok::<_, anyhow::Error>((n.clone(), v))
            }
        });
        for r in join_all(futs).await {
            let (n, v) = r?;
            self.packuments.insert(n, v);
        }
        Ok(())
    }

    pub async fn resolve(&mut self, root_specs: &[(String, String)], opts: &InstallOptions) -> Result<(Graph, HashMap<PkgId, Value>)> {
        let mut graph = Graph::default();
        let mut metas: HashMap<PkgId, Value> = HashMap::new();
        let mut resolved_cache: HashMap<(String, String), Option<PkgId>> = HashMap::new();
        let mut wave: Vec<(Option<PkgId>, String, String, bool)> =
            root_specs.iter().map(|(n, r)| (None, n.clone(), r.clone(), false)).collect();
        let mut visited: HashSet<PkgId> = HashSet::new();
        while !wave.is_empty() {
            let names: Vec<String> = wave.iter().map(|(_, n, r, _)| real_name(n, r).0).collect();
            self.load(names).await?;
            let mut next = Vec::new();
            for (parent, alias, range, optional) in wave {
                let (name, real_range) = real_name(&alias, &range);
                let key = (name.clone(), real_range.clone());
                let id = match resolved_cache.get(&key) {
                    Some(id) => id.clone(),
                    None => {
                        let p = &self.packuments[&name];
                        let id = pick_version(p, &real_range).map(|v| PkgId(format!("{name}@{v}")));
                        resolved_cache.insert(key, id.clone());
                        id
                    }
                };
                let Some(id) = id else {
                    if optional {
                        continue;
                    }
                    bail!("no version of {name} satisfies {real_range:?}");
                };
                let version = id.0[name.len() + 1..].to_string();
                let meta = self.packuments[&name]["versions"][&version].clone();
                let os = list(meta.get("os"));
                let cpu = list(meta.get("cpu"));
                let libc = list(meta.get("libc"));
                if !platform_matches(&os, &cpu, &libc, &opts.platform) {
                    if optional {
                        continue;
                    }
                    bail!("{} is not available for this platform", id.0);
                }
                match &parent {
                    None => {
                        graph.roots.insert(alias.clone(), id.clone());
                    }
                    Some(p) => {
                        graph.deps.entry(p.clone()).or_default().insert(alias.clone(), id.clone());
                    }
                }
                if visited.insert(id.clone()) {
                    graph.deps.entry(id.clone()).or_default();
                    for (k, opt) in [("dependencies", false), ("optionalDependencies", true)] {
                        if opt && !opts.include_optional {
                            continue;
                        }
                        if let Some(m) = meta.get(k).and_then(|d| d.as_object()) {
                            for (dn, dr) in m {
                                next.push((Some(id.clone()), dn.clone(), dr.as_str().unwrap_or("*").to_string(), opt));
                            }
                        }
                    }
                    metas.insert(id.clone(), meta);
                }
            }
            wave = next;
        }
        Ok((graph, metas))
    }
}

fn list(v: Option<&Value>) -> Option<Vec<String>> {
    match v? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()),
        _ => None,
    }
}

fn real_name(alias: &str, range: &str) -> (String, String) {
    if let Some(rest) = range.strip_prefix("npm:") {
        let at = rest[1..].find('@').map(|i| i + 1);
        return match at {
            Some(i) => (rest[..i].to_string(), rest[i + 1..].to_string()),
            None => (rest.to_string(), "*".to_string()),
        };
    }
    (alias.to_string(), range.to_string())
}

pub fn root_specs(pj: &Value, opts: &InstallOptions, skip: &HashSet<String>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
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
                if skip.contains(n) {
                    continue;
                }
                let r = r.as_str().unwrap_or("*");
                if r.starts_with("workspace:") || r.starts_with("file:") || r.starts_with("link:") {
                    continue;
                }
                if r.starts_with("git") || r.contains("github:") || r.starts_with("http") || (r.contains('/') && !r.starts_with("npm:")) {
                    bail!("dependency {n}@{r} needs a lockfile: git and URL dependencies are not resolved without one");
                }
                out.push((n.clone(), r.to_string()));
            }
        }
    }
    Ok(out)
}

pub async fn plan_without_lockfile(
    fetcher: &Fetcher,
    pj: &Value,
    workspaces: &[(String, Value)],
    opts: &InstallOptions,
) -> Result<InstallPlan> {
    let ws_names: BTreeMap<String, String> = workspaces
        .iter()
        .filter_map(|(dir, p)| p.get("name").and_then(|n| n.as_str()).map(|n| (n.to_string(), dir.clone())))
        .collect();
    let skip: HashSet<String> = ws_names.keys().cloned().collect();
    let mut specs = root_specs(pj, opts, &skip)?;
    for (_, p) in workspaces {
        specs.extend(root_specs(p, opts, &skip)?);
    }
    let mut resolver = Resolver::new(fetcher, None);
    let (graph, metas) = resolver.resolve(&specs, opts).await?;
    let layout = hoist(&graph);
    let mut plan = InstallPlan::default();
    let mut bin_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, id) in &layout {
        let meta = metas.get(id).ok_or_else(|| anyhow!("missing metadata for {}", id.0))?;
        let name = meta.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let version = meta.get("version").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let dist = meta.get("dist").ok_or_else(|| anyhow!("{} has no dist", id.0))?;
        let url = dist.get("tarball").and_then(|t| t.as_str()).ok_or_else(|| anyhow!("{} has no tarball", id.0))?.to_string();
        let integrity = match dist.get("integrity").and_then(|i| i.as_str()) {
            Some(i) => Some(Integrity::parse_sri(i)?),
            None => dist.get("shasum").and_then(|s| s.as_str()).map(|s| Integrity::parse_hex(acropolis_store::Algo::Sha1, s)).transpose()?,
        };
        if let Some((nm, _)) = parent_node_modules(path) {
            bin_dirs.entry(nm.to_string()).or_default().push(path.clone());
        }
        plan.packages.push(InstallPackage {
            path: path.clone(),
            name: name.clone(),
            version,
            source: Source::Registry { url, integrity },
            bins: Some(bins_of(&name, &meta.get("bin").cloned())),
            has_install_script: meta.get("hasInstallScript").and_then(|v| v.as_bool()).unwrap_or(false),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_latest_when_satisfying() {
        let p = serde_json::json!({
            "dist-tags": {"latest": "1.2.0", "next": "2.0.0-rc.1"},
            "versions": {"1.0.0": {}, "1.2.0": {}, "1.3.0": {}, "2.0.0-rc.1": {}}
        });
        assert_eq!(pick_version(&p, "^1.0.0").unwrap(), "1.2.0");
        assert_eq!(pick_version(&p, ">=1.3.0").unwrap(), "1.3.0");
        assert_eq!(pick_version(&p, "next").unwrap(), "2.0.0-rc.1");
        assert_eq!(pick_version(&p, "").unwrap(), "1.2.0");
        assert!(pick_version(&p, "^3").is_none());
        assert_eq!(real_name("str", "npm:string-width@^4"), ("string-width".to_string(), "^4".to_string()));
    }
}
