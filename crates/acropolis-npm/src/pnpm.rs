use crate::install::{
    BinDir, InstallOptions, InstallPackage, InstallPlan, Link, Source, default_registry_url, platform_matches_named,
    relative_link,
};
use acropolis_store::Integrity;
use anyhow::{Context, Result, bail};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

#[derive(Clone, Debug, Default)]
struct Pkg {
    name: String,
    version: String,
    integrity: Option<String>,
    tarball: Option<String>,
    directory: Option<String>,
    git: Option<String>,
    deps: BTreeMap<String, String>,
    optional_deps: BTreeMap<String, String>,
    os: Option<Vec<String>>,
    cpu: Option<Vec<String>>,
    libc: Option<Vec<String>>,
    has_bin: bool,
    requires_build: bool,
    optional: bool,
}

#[derive(Clone, Debug, Default)]
struct Importer {
    deps: BTreeMap<String, String>,
    dev_deps: BTreeMap<String, String>,
    optional_deps: BTreeMap<String, String>,
}

pub struct PnpmLock {
    importers: BTreeMap<String, Importer>,
    packages: BTreeMap<String, Pkg>,
}

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn str_list(v: Option<&Value>) -> Option<Vec<String>> {
    v.and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(s).collect())
}

fn dep_map(v: Option<&Value>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(m) = v.and_then(|v| v.as_mapping()) {
        for (k, val) in m {
            let Some(k) = s(k) else { continue };
            let version = match val {
                Value::Mapping(m) => m.get("version").and_then(s),
                other => s(other),
            };
            if let Some(version) = version {
                out.insert(k, version);
            }
        }
    }
    out
}

fn split_name_version(key: &str) -> (String, String) {
    let key = key.trim_start_matches('/');
    let base = match key.find('(') {
        Some(i) => &key[..i],
        None => key,
    };
    let at = base[1..].find('@').map(|i| i + 1);
    match at {
        Some(i) => (base[..i].to_string(), base[i + 1..].to_string()),
        None => (base.to_string(), String::new()),
    }
}

fn importer_of(v: &Value) -> Importer {
    Importer {
        deps: dep_map(v.get("dependencies")),
        dev_deps: dep_map(v.get("devDependencies")),
        optional_deps: dep_map(v.get("optionalDependencies")),
    }
}

impl PnpmLock {
    pub fn parse(text: &str) -> Result<Self> {
        let root: Value = serde_yaml::from_str(text).context("parsing pnpm-lock.yaml")?;
        let version = root.get("lockfileVersion").and_then(s).unwrap_or_default();
        let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
        if major < 5 {
            bail!("pnpm lockfile version {version} is too old; regenerate it with pnpm 8 or newer");
        }
        let mut importers = BTreeMap::new();
        if let Some(m) = root.get("importers").and_then(|v| v.as_mapping()) {
            for (k, v) in m {
                if let Some(k) = s(k) {
                    importers.insert(k, importer_of(v));
                }
            }
        } else {
            importers.insert(".".to_string(), importer_of(&root));
        }
        let mut packages: BTreeMap<String, Pkg> = BTreeMap::new();
        let mut meta: HashMap<String, Value> = HashMap::new();
        if let Some(m) = root.get("packages").and_then(|v| v.as_mapping()) {
            for (k, v) in m {
                if let Some(k) = s(k) {
                    meta.insert(k.trim_start_matches('/').to_string(), v.clone());
                }
            }
        }
        let snapshots = root.get("snapshots").and_then(|v| v.as_mapping()).cloned();
        let entries: Vec<(String, Value)> = match &snapshots {
            Some(snap) => snap.iter().filter_map(|(k, v)| s(k).map(|k| (k, v.clone()))).collect(),
            None => meta.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
        for (key, snap) in entries {
            let key = key.trim_start_matches('/').to_string();
            let (name0, version0) = split_name_version(&key);
            let base_key = match key.find('(') {
                Some(i) => key[..i].to_string(),
                None => key.clone(),
            };
            let m = meta
                .get(&base_key)
                .or_else(|| meta.get(&key))
                .cloned()
                .unwrap_or(Value::Null);
            let name = m.get("name").and_then(s).unwrap_or(name0);
            let version = m.get("version").and_then(s).unwrap_or(version0);
            let res = m.get("resolution");
            let pkg = Pkg {
                name,
                version,
                integrity: res.and_then(|r| r.get("integrity")).and_then(s),
                tarball: res.and_then(|r| r.get("tarball")).and_then(s),
                directory: res.and_then(|r| r.get("directory")).and_then(s),
                git: res.and_then(|r| {
                    let repo = r.get("repo").and_then(s)?;
                    let commit = r.get("commit").and_then(s).unwrap_or_default();
                    Some(format!("{repo}#{commit}"))
                }),
                deps: dep_map(snap.get("dependencies")),
                optional_deps: dep_map(snap.get("optionalDependencies")),
                os: str_list(m.get("os")),
                cpu: str_list(m.get("cpu")),
                libc: str_list(m.get("libc")),
                has_bin: m.get("hasBin").and_then(|v| v.as_bool()).unwrap_or(false),
                requires_build: m.get("requiresBuild").and_then(|v| v.as_bool()).unwrap_or(false),
                optional: snap.get("optional").and_then(|v| v.as_bool()).unwrap_or(false)
                    || m.get("optional").and_then(|v| v.as_bool()).unwrap_or(false),
            };
            packages.insert(key, pkg);
        }
        Ok(PnpmLock { importers, packages })
    }

    fn resolve_ref(&self, name: &str, reference: &str) -> Option<String> {
        if reference.starts_with("link:") {
            return None;
        }
        let r = reference.trim_start_matches('/');
        let candidate = if r.starts_with(|c: char| c.is_ascii_digit()) {
            format!("{name}@{r}")
        } else {
            r.to_string()
        };
        if self.packages.contains_key(&candidate) {
            return Some(candidate);
        }
        let alt = format!("{name}@{r}");
        if self.packages.contains_key(&alt) {
            return Some(alt);
        }
        None
    }

    pub fn install_plan(&self, opts: &InstallOptions, workspace_dirs: &[String]) -> Result<InstallPlan> {
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut skipped_platform = Vec::new();
        for (path, imp) in &self.importers {
            if path != "." && !workspace_dirs.iter().any(|w| w == path) && !workspace_dirs.is_empty() {
                continue;
            }
            let mut roots: Vec<(&String, &String)> = imp.deps.iter().collect();
            if opts.include_optional {
                roots.extend(imp.optional_deps.iter());
            }
            if opts.include_dev {
                roots.extend(imp.dev_deps.iter());
            }
            for (name, r) in roots {
                if let Some(dp) = self.resolve_ref(name, r) {
                    queue.push_back(dp);
                }
            }
        }
        while let Some(dp) = queue.pop_front() {
            if reachable.contains(&dp) {
                continue;
            }
            let Some(p) = self.packages.get(&dp) else { continue };
            if !platform_matches_named(&p.name, &p.os, &p.cpu, &p.libc, &opts.platform) {
                skipped_platform.push(dp.clone());
                continue;
            }
            if p.optional && !opts.include_optional {
                continue;
            }
            reachable.insert(dp.clone());
            let mut children: Vec<(&String, &String)> = p.deps.iter().collect();
            if opts.include_optional {
                children.extend(p.optional_deps.iter());
            }
            for (n, r) in children {
                if let Some(c) = self.resolve_ref(n, r) {
                    queue.push_back(c);
                }
            }
        }
        let mut plan = InstallPlan {
            skipped_platform,
            ..Default::default()
        };
        let store_dir = |dp: &str| -> String {
            let p = &self.packages[dp];
            format!(
                "node_modules/.pnpm/{}/node_modules/{}",
                dep_path_to_filename(dp),
                p.name
            )
        };
        let mut hoisted: BTreeMap<String, String> = BTreeMap::new();
        for dp in &reachable {
            let p = &self.packages[dp];
            let path = store_dir(dp);
            let source = if let Some(dir) = &p.directory {
                plan.links.push(Link {
                    path: path.clone(),
                    target: relative_link(Path::new(&path), Path::new(dir)),
                });
                None
            } else if let Some(g) = &p.git {
                Some(Source::Git { url: g.clone() })
            } else {
                let url = match &p.tarball {
                    Some(t) if t.starts_with("http") => t.clone(),
                    Some(t) if t.starts_with("file:") => {
                        bail!("local tarball dependency {} ({t}) is not supported yet", p.name)
                    }
                    _ => default_registry_url(&p.name, &p.version),
                };
                let integrity = match &p.integrity {
                    Some(i) => Some(Integrity::parse_sri(i).with_context(|| format!("integrity of {dp}"))?),
                    None => None,
                };
                Some(Source::Registry { url, integrity })
            };
            if let Some(source) = source {
                plan.packages.push(InstallPackage {
                    path: path.clone(),
                    name: p.name.clone(),
                    version: p.version.clone(),
                    source,
                    bins: if p.has_bin { None } else { Some(Vec::new()) },
                    has_install_script: p.requires_build,
                    optional: p.optional,
                    dev: false,
                });
            }
            let parent_nm = format!("node_modules/.pnpm/{}/node_modules", dep_path_to_filename(dp));
            let mut bin_pkgs = Vec::new();
            let mut children: Vec<(&String, &String)> = p.deps.iter().collect();
            children.extend(p.optional_deps.iter());
            for (alias, r) in children {
                let Some(c) = self.resolve_ref(alias, r) else { continue };
                if !reachable.contains(&c) {
                    continue;
                }
                let link_path = format!("{parent_nm}/{alias}");
                let target = store_dir(&c);
                plan.links.push(Link {
                    path: link_path,
                    target: relative_link(Path::new(&format!("{parent_nm}/{alias}")), Path::new(&target)),
                });
                bin_pkgs.push(target);
            }
            if !bin_pkgs.is_empty() {
                plan.bin_dirs.push(BinDir {
                    dir: format!("{path}/node_modules"),
                    packages: bin_pkgs,
                });
            }
            let entry = hoisted.entry(p.name.clone()).or_insert_with(|| dp.clone());
            if version_key(&self.packages[entry.as_str()].version) < version_key(&p.version) {
                *entry = dp.clone();
            }
        }
        for (name, dp) in &hoisted {
            let link_path = format!("node_modules/.pnpm/node_modules/{name}");
            let target = store_dir(dp);
            plan.links.push(Link {
                path: link_path.clone(),
                target: relative_link(Path::new(&link_path), Path::new(&target)),
            });
        }
        for (ipath, imp) in &self.importers {
            if ipath != "." && !workspace_dirs.is_empty() && !workspace_dirs.iter().any(|w| w == ipath) {
                continue;
            }
            let nm = if ipath == "." {
                "node_modules".to_string()
            } else {
                format!("{ipath}/node_modules")
            };
            let mut direct: Vec<(&String, &String)> = imp.deps.iter().collect();
            if opts.include_optional {
                direct.extend(imp.optional_deps.iter());
            }
            if opts.include_dev {
                direct.extend(imp.dev_deps.iter());
            }
            let mut bin_pkgs = Vec::new();
            for (alias, r) in direct {
                let link_path = format!("{nm}/{alias}");
                if let Some(local) = r.strip_prefix("link:") {
                    let base = if ipath == "." {
                        String::new()
                    } else {
                        format!("{ipath}/")
                    };
                    let target_path = normalize(&format!("{base}{local}"));
                    plan.links.push(Link {
                        path: link_path.clone(),
                        target: relative_link(Path::new(&link_path), Path::new(&target_path)),
                    });
                    plan.known_bins.entry(link_path.clone()).or_default();
                    bin_pkgs.push(link_path);
                    continue;
                }
                let Some(dp) = self.resolve_ref(alias, r) else { continue };
                if !reachable.contains(&dp) {
                    continue;
                }
                let target = store_dir(&dp);
                plan.links.push(Link {
                    path: link_path.clone(),
                    target: relative_link(Path::new(&link_path), Path::new(&target)),
                });
                bin_pkgs.push(target);
            }
            if !bin_pkgs.is_empty() {
                plan.bin_dirs.push(BinDir {
                    dir: nm,
                    packages: bin_pkgs,
                });
            }
        }
        plan.links.sort_by(|a, b| a.path.cmp(&b.path));
        plan.links.dedup_by(|a, b| a.path == b.path);
        Ok(plan)
    }
}

fn normalize(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn version_key(v: &str) -> (u64, u64, u64, String) {
    let core = v.split(['-', '+', '(']).next().unwrap_or("");
    let mut it = core.split('.').map(|x| x.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        v.to_string(),
    )
}

pub fn dep_path_to_filename(dep_path: &str) -> String {
    let unescaped = {
        let d = dep_path.trim_start_matches('/');
        if d.starts_with("file:") {
            d.replacen(':', "+", 1)
        } else {
            d.to_string()
        }
    };
    let mut filename: String = unescaped
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '+'
            } else {
                c
            }
        })
        .collect();
    if filename.contains('(') {
        if filename.ends_with(')') {
            filename.pop();
        }
        filename = filename.replace(")(", "_").replace(['(', ')'], "_");
    }
    let max = 120;
    if filename.len() > max || (filename != filename.to_lowercase() && !filename.starts_with("file+")) {
        let hash = base32_md5(&filename);
        let keep = max - 27;
        let cut = filename
            .char_indices()
            .nth(keep)
            .map(|(i, _)| i)
            .unwrap_or(filename.len());
        return format!("{}_{}", &filename[..cut], hash);
    }
    filename
}

fn base32_md5(s: &str) -> String {
    let digest = md5_digest(s.as_bytes());
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in &digest {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(alphabet[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(alphabet[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn md5_digest(data: &[u8]) -> [u8; 16] {
    let s: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    for chunk in msg.chunks(64) {
        let m: Vec<u32> = (0..16)
            .map(|i| u32::from_le_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(s[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..].copy_from_slice(&d0.to_le_bytes());
    out
}

pub fn workspace_globs(app_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(app_dir.join("pnpm-workspace.yaml")) else {
        return Vec::new();
    };
    let Ok(v) = serde_yaml::from_str::<Value>(&text) else {
        return Vec::new();
    };
    str_list(v.get("packages")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known() {
        assert_eq!(hex_of(&md5_digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            hex_of(&md5_digest(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    fn hex_of(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn filenames() {
        assert_eq!(
            dep_path_to_filename("react-dom@18.3.1(react@18.3.1)"),
            "react-dom@18.3.1_react@18.3.1"
        );
        assert_eq!(dep_path_to_filename("/@babel/core@7.0.0"), "@babel+core@7.0.0");
        assert_eq!(
            dep_path_to_filename("@sveltejs/vite-plugin-svelte@5.0.3(svelte@5.20.2)(vite@6.2.0)"),
            "@sveltejs+vite-plugin-svelte@5.0.3_svelte@5.20.2_vite@6.2.0"
        );
    }

    #[test]
    fn parse_v9_workspace() {
        let text = "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      pkg-a:\n        specifier: workspace:*\n        version: link:packages/pkg-a\n  packages/pkg-a:\n    dependencies:\n      abbrev:\n        specifier: ^3.0.0\n        version: 3.0.1\npackages:\n  abbrev@3.0.1:\n    resolution: {integrity: sha512-AO2ac6pjRB3SJmGJo+v5/aK6Omggp6fsLrs6wN9bd35ulu4cCwaAU9+7ZhXjeqHVkaHThLuzH0nZr0YpCDhygg==}\nsnapshots:\n  abbrev@3.0.1: {}\n";
        let lock = PnpmLock::parse(text).unwrap();
        let plan = lock
            .install_plan(
                &InstallOptions {
                    include_dev: true,
                    include_optional: true,
                    platform: Default::default(),
                },
                &[],
            )
            .unwrap();
        assert_eq!(plan.packages.len(), 1);
        assert_eq!(
            plan.packages[0].path,
            "node_modules/.pnpm/abbrev@3.0.1/node_modules/abbrev"
        );
        let paths: Vec<&str> = plan.links.iter().map(|l| l.path.as_str()).collect();
        assert!(paths.contains(&"node_modules/pkg-a"));
        assert!(paths.contains(&"packages/pkg-a/node_modules/abbrev"));
        let l = plan
            .links
            .iter()
            .find(|l| l.path == "packages/pkg-a/node_modules/abbrev")
            .unwrap();
        assert_eq!(l.target, "../../../node_modules/.pnpm/abbrev@3.0.1/node_modules/abbrev");
    }
}
