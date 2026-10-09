#[cfg(test)]
use crate::lockfile::LockEntry;
use crate::lockfile::{PackageLock, bins_of, package_name_from_path};
use acropolis_fetch::Fetcher;
use acropolis_oci::tar::{Kind, TarReader, TarWriter, normalize_mode};
use acropolis_store::{Integrity, StoredBlob};
use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use futures::future::try_join_all;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub type Bins = Vec<(String, String)>;

#[derive(Clone, Debug)]
pub struct Platform {
    pub os: String,
    pub cpu: String,
    pub libc: String,
}

impl Default for Platform {
    fn default() -> Self {
        let cpu = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        };
        Platform {
            os: "linux".into(),
            cpu: cpu.into(),
            libc: "glibc".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub include_dev: bool,
    pub include_optional: bool,
    pub platform: Platform,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Registry { url: String, integrity: Option<Integrity> },
    Link { target: String },
    Git { url: String },
    Bundled,
}

#[derive(Clone, Debug)]
pub struct InstallPackage {
    pub path: String,
    pub name: String,
    pub version: String,
    pub source: Source,
    pub bins: Option<Vec<(String, String)>>,
    pub has_install_script: bool,
    pub optional: bool,
    pub dev: bool,
}

#[derive(Clone, Debug)]
pub struct Link {
    pub path: String,
    pub target: String,
}

#[derive(Clone, Debug, Default)]
pub struct BinDir {
    pub dir: String,
    pub packages: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct InstallPlan {
    pub packages: Vec<InstallPackage>,
    pub links: Vec<Link>,
    pub bin_dirs: Vec<BinDir>,
    pub known_bins: HashMap<String, Vec<(String, String)>>,
    pub skipped_platform: Vec<String>,
}

fn matches_list(list: &Option<Vec<String>>, value: &str) -> bool {
    let Some(list) = list else { return true };
    if list.is_empty() {
        return true;
    }
    let negs: Vec<&str> = list.iter().filter_map(|s| s.strip_prefix('!')).collect();
    if negs.contains(&value) {
        return false;
    }
    let pos: Vec<&String> = list.iter().filter(|s| !s.starts_with('!')).collect();
    pos.is_empty() || pos.iter().any(|s| s.as_str() == value || s.as_str() == "any")
}

pub fn platform_matches(
    os: &Option<Vec<String>>,
    cpu: &Option<Vec<String>>,
    libc: &Option<Vec<String>>,
    p: &Platform,
) -> bool {
    matches_list(os, &p.os) && matches_list(cpu, &p.cpu) && matches_list(libc, &p.libc)
}

pub fn inferred_libc(name: &str, libc: &Option<Vec<String>>) -> Option<Vec<String>> {
    if libc.is_some() {
        return libc.clone();
    }
    let short = name.rsplit('/').next().unwrap_or(name);
    if short.contains("linuxmusl") || short.ends_with("-musl") || short.contains("-musl-") {
        return Some(vec!["musl".into()]);
    }
    if short.ends_with("-gnu") || short.contains("-gnu-") {
        return Some(vec!["glibc".into()]);
    }
    None
}

const NAME_OS: &[(&str, &str)] = &[
    ("darwin", "darwin"),
    ("win32", "win32"),
    ("windows", "win32"),
    ("freebsd", "freebsd"),
    ("openbsd", "openbsd"),
    ("netbsd", "netbsd"),
    ("sunos", "sunos"),
    ("android", "android"),
    ("aix", "aix"),
    ("linux", "linux"),
    ("linuxmusl", "linux"),
    ("webcontainers", "webcontainers"),
];
const NAME_CPU: &[&str] = &[
    "x64",
    "arm64",
    "arm",
    "ia32",
    "ppc64",
    "ppc64le",
    "s390x",
    "riscv64",
    "loong64",
    "mips64el",
    "wasm32",
    "universal",
];

pub fn inferred_os_cpu(name: &str) -> (Option<Vec<String>>, Option<Vec<String>>) {
    let short = name.rsplit('/').next().unwrap_or(name);
    let parts: Vec<&str> = short.split('-').collect();
    for (i, w) in parts.iter().enumerate() {
        let Some(&(_, os)) = NAME_OS.iter().find(|(token, _)| token == w) else {
            continue;
        };
        let cpu = parts.get(i + 1).filter(|c| NAME_CPU.contains(c));
        if let Some(cpu) = cpu {
            let cpu = if *cpu == "universal" {
                "x64".to_string()
            } else {
                cpu.to_string()
            };
            let cpus = if short.contains("universal") {
                vec!["x64".to_string(), "arm64".to_string()]
            } else {
                vec![cpu]
            };
            return (Some(vec![os.to_string()]), Some(cpus));
        }
    }
    if parts.len() >= 2 && parts[parts.len() - 1] == "wasm32" {
        return (None, Some(vec!["wasm32".to_string()]));
    }
    (None, None)
}

pub fn platform_matches_named(
    name: &str,
    os: &Option<Vec<String>>,
    cpu: &Option<Vec<String>>,
    libc: &Option<Vec<String>>,
    p: &Platform,
) -> bool {
    let (ios, icpu) = if os.is_none() && cpu.is_none() {
        inferred_os_cpu(name)
    } else {
        (None, None)
    };
    platform_matches(
        &os.clone().or(ios),
        &cpu.clone().or(icpu),
        &inferred_libc(name, libc),
        p,
    )
}

#[cfg(test)]
fn platform_ok(e: &LockEntry, p: &Platform) -> bool {
    let name = e.name.clone().unwrap_or_default();
    platform_matches_named(&name, &e.os, &e.cpu, &e.libc, p)
}

pub fn parent_node_modules(path: &str) -> Option<(&str, &str)> {
    let idx = path.rfind("node_modules/")?;
    Some((
        &path[..idx + "node_modules".len()],
        &path[idx + "node_modules/".len()..],
    ))
}

impl InstallPlan {
    pub fn from_lock(lock: &PackageLock, opts: &InstallOptions) -> Result<Self> {
        let mut out = InstallPlan::default();
        let mut skipped: Vec<String> = Vec::new();
        let mut bin_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, e) in &lock.packages {
            if path.is_empty() || !path.contains("node_modules/") {
                if !path.is_empty() {
                    let bins = bins_of(e.name.as_deref().unwrap_or(""), &e.bin);
                    if !bins.is_empty() {
                        out.known_bins.insert(path.clone(), bins);
                    }
                }
                continue;
            }
            if skipped
                .iter()
                .any(|s| path.strip_prefix(s.as_str()).is_some_and(|rest| rest.starts_with('/')))
            {
                continue;
            }
            let keep = if e.dev_optional {
                opts.include_dev || opts.include_optional
            } else {
                (!e.dev || opts.include_dev) && (!e.optional || opts.include_optional)
            };
            if !keep {
                skipped.push(path.clone());
                continue;
            }
            let name = e
                .name
                .clone()
                .unwrap_or_else(|| package_name_from_path(path).to_string());
            if !platform_matches_named(&name, &e.os, &e.cpu, &e.libc, &opts.platform) {
                skipped.push(path.clone());
                out.skipped_platform.push(path.clone());
                continue;
            }
            if let Some((nm, _)) = parent_node_modules(path) {
                bin_dirs.entry(nm.to_string()).or_default().push(path.clone());
            }
            if e.link {
                let target = e
                    .resolved
                    .clone()
                    .ok_or_else(|| anyhow!("link {path} without resolved"))?;
                if let Some(t) = lock.packages.get(&target) {
                    let bins = bins_of(&name, &t.bin);
                    if !bins.is_empty() {
                        out.known_bins.insert(path.clone(), bins);
                    }
                }
                out.links.push(Link {
                    path: path.clone(),
                    target: relative_link(Path::new(path), Path::new(&target)),
                });
                continue;
            }
            let source = if e.in_bundle {
                Source::Bundled
            } else {
                let resolved = e.resolved.clone().unwrap_or_default();
                if resolved.starts_with("git+") || resolved.starts_with("github:") || resolved.starts_with("git:") {
                    Source::Git { url: resolved }
                } else if resolved.starts_with("http://") || resolved.starts_with("https://") {
                    let integrity = match &e.integrity {
                        Some(i) => Some(Integrity::parse_sri(i).with_context(|| format!("integrity of {path}"))?),
                        None => None,
                    };
                    Source::Registry {
                        url: resolved,
                        integrity,
                    }
                } else if let Some(target) = resolved.strip_prefix("file:") {
                    out.links.push(Link {
                        path: path.clone(),
                        target: relative_link(Path::new(path), Path::new(target)),
                    });
                    continue;
                } else if resolved.is_empty() {
                    let version = e.version.clone().unwrap_or_default();
                    if version.is_empty() {
                        bail!("{path} has no resolved URL or version in package-lock.json");
                    }
                    let integrity = match &e.integrity {
                        Some(i) => Some(Integrity::parse_sri(i)?),
                        None => None,
                    };
                    Source::Registry {
                        url: default_registry_url(&name, &version),
                        integrity,
                    }
                } else {
                    bail!("unsupported resolved URL for {path}: {resolved}");
                }
            };
            out.packages.push(InstallPackage {
                path: path.clone(),
                name: name.clone(),
                version: e.version.clone().unwrap_or_default(),
                source,
                bins: Some(bins_of(&name, &e.bin)),
                has_install_script: e.has_install_script,
                optional: e.optional || e.dev_optional,
                dev: e.dev,
            });
        }
        out.bin_dirs = bin_dirs
            .into_iter()
            .map(|(dir, packages)| BinDir { dir, packages })
            .collect();
        Ok(out)
    }

    pub fn check_paths(&self) -> Result<()> {
        for path in self
            .packages
            .iter()
            .map(|p| &p.path)
            .chain(self.links.iter().map(|l| &l.path))
        {
            safe_rel_path(path)?;
        }
        Ok(())
    }
}

pub fn default_registry_url(name: &str, version: &str) -> String {
    let short = name.rsplit('/').next().unwrap_or(name);
    format!("https://registry.npmjs.org/{name}/-/{short}-{version}.tgz")
}

pub type Tarballs = HashMap<String, StoredBlob>;

pub fn git_tarball_url(url: &str) -> Option<String> {
    let (repo, commit) = url.rsplit_once('#')?;
    if commit.len() < 7 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let repo = repo
        .trim_start_matches("git+")
        .trim_start_matches("ssh://")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("git://")
        .trim_start_matches("git@");
    let path = if let Some(rest) = repo.strip_prefix("github:") {
        rest.to_string()
    } else if let Some(rest) = repo
        .strip_prefix("github.com/")
        .or_else(|| repo.strip_prefix("github.com:"))
    {
        rest.to_string()
    } else if !repo.contains(':') && !repo.contains('.') && repo.matches('/').count() == 1 {
        repo.to_string()
    } else {
        return None;
    };
    let path = path.trim_end_matches(".git");
    Some(format!("https://codeload.github.com/{path}/tar.gz/{commit}"))
}

pub async fn fetch_all(fetcher: &Fetcher, plan: &InstallPlan) -> Result<Tarballs> {
    let mut unique: BTreeMap<String, Option<Integrity>> = BTreeMap::new();
    for p in &plan.packages {
        match &p.source {
            Source::Registry { url, integrity } => {
                unique.entry(url.clone()).or_insert_with(|| integrity.clone());
            }
            Source::Git { url } => {
                let t = git_tarball_url(url).ok_or_else(|| {
                    anyhow!(
                        "git dependency {} ({url}) is only supported for GitHub repositories pinned to a commit",
                        p.path
                    )
                })?;
                unique.entry(t).or_insert(None);
            }
            _ => {}
        }
    }
    let futs = unique.into_iter().map(|(url, integrity)| async move {
        let what = url.rsplit('/').next().unwrap_or(&url).to_string();
        let blob = fetcher.blob(&what, &url, integrity).await?;
        Ok::<_, anyhow::Error>((url, blob))
    });
    let results = try_join_all(futs).await?;
    Ok(results.into_iter().collect())
}

pub fn blob_for<'a>(tarballs: &'a Tarballs, p: &InstallPackage) -> Option<&'a StoredBlob> {
    match &p.source {
        Source::Registry { url, .. } => tarballs.get(url),
        Source::Git { url } => git_tarball_url(url).and_then(|t| tarballs.get(&t)),
        _ => None,
    }
}

fn fetched(p: &InstallPackage) -> bool {
    matches!(p.source, Source::Registry { .. } | Source::Git { .. })
}

fn clean_rel(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => return None,
            c => parts.push(c),
        }
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

pub struct TarEntry {
    pub rel: String,
    pub kind: Kind,
    pub mode: u32,
    pub data: Vec<u8>,
}

pub fn read_tarball(blob: &Path) -> Result<Vec<TarEntry>> {
    let mut tr = open_tarball(blob)?;
    let mut out = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    while let Some(e) = tr.next_entry()? {
        if !matches!(e.kind, Kind::File | Kind::Dir) {
            continue;
        }
        let rel = match e.path.split_once('/') {
            Some((_, rest)) => rest,
            None => continue,
        };
        let Some(rel) = clean_rel(rel) else { continue };
        let data = if e.kind == Kind::File {
            tr.read_data()?
        } else {
            Vec::new()
        };
        if let Some(&i) = seen.get(&rel) {
            out[i] = TarEntry {
                rel,
                kind: e.kind,
                mode: e.mode,
                data,
            };
            continue;
        }
        seen.insert(rel.clone(), out.len());
        out.push(TarEntry {
            rel,
            kind: e.kind,
            mode: e.mode,
            data,
        });
    }
    Ok(out)
}

pub fn bins_from_package_json(name: &str, data: &[u8], files: &[TarEntry]) -> Bins {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let bins = bins_of(name, &v.get("bin").cloned());
    if !bins.is_empty() {
        return bins;
    }
    if let Some(dir) = v.get("directories").and_then(|d| d.get("bin")).and_then(|b| b.as_str())
        && let Some(dir) = clean_rel(dir)
    {
        let prefix = format!("{dir}/");
        return files
            .iter()
            .filter(|f| f.kind == Kind::File && f.rel.starts_with(&prefix) && !f.rel[prefix.len()..].contains('/'))
            .map(|f| (f.rel[prefix.len()..].to_string(), f.rel.clone()))
            .collect();
    }
    Vec::new()
}

fn package_bins(p: &InstallPackage, entries: &[TarEntry]) -> Bins {
    match &p.bins {
        Some(b) => b.clone(),
        None => entries
            .iter()
            .find(|e| e.rel == "package.json")
            .map(|e| bins_from_package_json(&p.name, &e.data, entries))
            .unwrap_or_default(),
    }
}

fn package_fragment(prefix: &str, p: &InstallPackage, blob: &Path) -> Result<(Vec<u8>, Bins)> {
    let entries = read_tarball(blob).with_context(|| format!("reading tarball of {}", p.path))?;
    let bins = package_bins(p, &entries);
    let execs: HashSet<String> = bins.iter().filter_map(|(_, t)| clean_rel(t)).collect();
    let cap: usize = entries.iter().map(|e| e.data.len() + 1024).sum();
    let mut tw = TarWriter::new(Vec::with_capacity(cap));
    let mut dirs: HashSet<String> = HashSet::new();
    let root = if prefix.is_empty() {
        p.path.clone()
    } else {
        format!("{prefix}/{}", p.path)
    };
    tw.dir(&root, 0o755)?;
    dirs.insert(root.clone());
    for e in &entries {
        let parts: Vec<&str> = e.rel.split('/').collect();
        let upto = if e.kind == Kind::Dir {
            parts.len()
        } else {
            parts.len() - 1
        };
        let mut acc = root.clone();
        for part in &parts[..upto] {
            acc.push('/');
            acc.push_str(part);
            if dirs.insert(acc.clone()) {
                tw.dir(&acc, 0o755)?;
            }
        }
        if e.kind == Kind::File {
            let mode = if execs.contains(&e.rel) {
                0o755
            } else {
                normalize_mode(e.mode, Kind::File)
            };
            tw.file_bytes(&format!("{root}/{}", e.rel), mode, &e.data)?;
        }
    }
    Ok((take(tw), bins))
}

fn take(mut tw: TarWriter<Vec<u8>>) -> Vec<u8> {
    std::mem::take(tw.get_mut())
}

fn bin_links(plan: &InstallPlan, bins: &HashMap<String, Bins>) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let link_targets: HashMap<&str, &str> = plan
        .links
        .iter()
        .map(|l| (l.path.as_str(), l.target.as_str()))
        .collect();
    for bd in &plan.bin_dirs {
        for pkg in &bd.packages {
            let list = bins.get(pkg).or_else(|| plan.known_bins.get(pkg));
            let Some(list) = list else { continue };
            let real = match link_targets.get(pkg.as_str()) {
                Some(t) => normalize_join(Path::new(pkg).parent().unwrap_or(Path::new("")), t),
                None => pkg.clone(),
            };
            for (name, target) in list {
                let Some(t) = clean_rel(target) else { continue };
                if name.contains('/') || name.is_empty() || name == "." || name == ".." {
                    continue;
                }
                let from = format!("{}/.bin/{name}", bd.dir);
                let to = format!("{real}/{t}");
                out.entry(bd.dir.clone())
                    .or_default()
                    .entry(name.clone())
                    .or_insert_with(|| relative_link(Path::new(&from), Path::new(&to)));
            }
        }
    }
    out
}

fn normalize_join(base: &Path, rel: &str) -> String {
    let mut parts: Vec<String> = base
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for c in rel.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    parts.join("/")
}

pub fn stream_node_modules(
    plan: &InstallPlan,
    tarballs: &Tarballs,
    prefix: &str,
    filter: impl Fn(&InstallPackage) -> bool + Sync,
    sink: &mut dyn FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
    plan.check_paths()?;
    let mut pkgs: Vec<&InstallPackage> = plan.packages.iter().filter(|p| filter(p)).collect();
    pkgs.sort_by(|a, b| a.path.cmp(&b.path));
    let mut head_dirs = BTreeSet::new();
    let mut acc = String::new();
    for c in prefix.split('/').filter(|c| !c.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(c);
        head_dirs.insert(acc.clone());
    }
    let base = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };
    let mut ancestors = |path: &str| {
        let parts: Vec<&str> = path.split('/').collect();
        let mut acc = prefix.to_string();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            head_dirs.insert(acc.clone());
        }
    };
    for p in &pkgs {
        ancestors(&p.path);
    }
    for l in &plan.links {
        ancestors(&l.path);
    }
    let mut head = TarWriter::new(Vec::new());
    for d in &head_dirs {
        head.dir(d, 0o755)?;
    }
    let mut links = plan.links.clone();
    links.sort_by(|a, b| a.path.cmp(&b.path));
    for l in &links {
        head.symlink(&format!("{base}{}", l.path), &l.target)?;
    }
    sink(take(head))?;
    let wanted: Vec<&InstallPackage> = pkgs.iter().copied().filter(|p| fetched(p)).collect();
    let window = (rayon::current_num_threads() * 2).max(4);
    let mut bins: HashMap<String, Bins> = HashMap::new();
    for win in wanted.chunks(window) {
        let bodies: Vec<Result<(String, Vec<u8>, Bins)>> = win
            .par_iter()
            .map(|p| {
                let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
                let (frag, bins) = package_fragment(prefix, p, &blob.path)?;
                Ok((p.path.clone(), frag, bins))
            })
            .collect();
        for b in bodies {
            let (path, frag, b) = b?;
            sink(frag)?;
            if !b.is_empty() {
                bins.insert(path, b);
            }
        }
    }
    let mut tail = TarWriter::new(Vec::new());
    for (dir, entries) in bin_links(plan, &bins) {
        tail.dir(&format!("{base}{dir}/.bin"), 0o755)?;
        for (name, target) in entries {
            tail.symlink(&format!("{base}{dir}/.bin/{name}"), &target)?;
        }
    }
    sink(take(tail))?;
    Ok(())
}

pub fn relative_link(from: &Path, to: &Path) -> String {
    let from_dir: Vec<_> = from.parent().map(|p| p.components().collect()).unwrap_or_default();
    let to_c: Vec<_> = to.components().collect();
    let mut common = 0;
    while common < from_dir.len() && common < to_c.len() && from_dir[common] == to_c[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..from_dir.len() {
        parts.push("..".into());
    }
    for c in &to_c[common..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

const TYPE_FILES: &[&str] = &[".d.ts", ".d.mts", ".d.cts", ".ts", ".tsx", ".mts", ".cts", ".json"];

fn type_files_only(entries: &[TarEntry], bins: &[(String, String)]) -> bool {
    if !bins.is_empty() {
        return false;
    }
    let declared = entries
        .iter()
        .find(|e| e.rel == "package.json")
        .and_then(|e| serde_json::from_slice::<serde_json::Value>(&e.data).ok())
        .map(|v| v.get("types").is_some() || v.get("typings").is_some())
        .unwrap_or(false);
    declared
        || entries
            .iter()
            .any(|e| e.rel.ends_with(".d.ts") || e.rel.ends_with(".d.mts") || e.rel.ends_with(".d.cts"))
}

pub fn safe_rel_path(path: &str) -> Result<()> {
    if path.is_empty() || path.starts_with('/') || path.contains('\0') || path.contains('\\') {
        bail!("unsafe install path {path:?}");
    }
    if path.split('/').any(|c| c == "..") {
        bail!("unsafe install path {path:?}");
    }
    Ok(())
}

/// Confines writes to a root: every directory is checked after symlink resolution.
pub struct Inside {
    root: PathBuf,
    ok: Mutex<HashSet<PathBuf>>,
}

impl Inside {
    pub fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Inside {
            root: std::fs::canonicalize(root)?,
            ok: Mutex::new(HashSet::new()),
        })
    }

    /// True if `path` exists and, with every symlink resolved, stays inside the root.
    pub fn contains(&self, path: &Path) -> bool {
        std::fs::canonicalize(path).is_ok_and(|real| real.starts_with(&self.root))
    }

    pub fn dir(&self, dir: &Path) -> Result<()> {
        if self.ok.lock().unwrap_or_else(|e| e.into_inner()).contains(dir) {
            return Ok(());
        }
        let mut existing = dir;
        while std::fs::symlink_metadata(existing).is_err() {
            existing = existing
                .parent()
                .ok_or_else(|| anyhow!("no existing ancestor for {}", dir.display()))?;
        }
        let real = std::fs::canonicalize(existing)?;
        if !real.starts_with(&self.root) {
            bail!(
                "refusing to write outside the install root: {} resolves to {}",
                dir.display(),
                real.display()
            );
        }
        std::fs::create_dir_all(dir)?;
        let real = std::fs::canonicalize(dir)?;
        if !real.starts_with(&self.root) {
            bail!(
                "refusing to write outside the install root: {} resolves to {}",
                dir.display(),
                real.display()
            );
        }
        self.ok
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_path_buf());
        Ok(())
    }
}

pub fn materialize_with(plan: &InstallPlan, tarballs: &Tarballs, root: &Path, types_only: bool) -> Result<u64> {
    plan.check_paths()?;
    let inside = Inside::new(root)?;
    let pkgs: Vec<&InstallPackage> = plan.packages.iter().collect();
    let written: Vec<Result<(String, u64, Bins)>> = pkgs
        .par_iter()
        .filter(|p| fetched(p))
        .map(|p| {
            let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
            let dest = root.join(&p.path);
            let (n, bins) = if types_only {
                extract_package_with(p, &blob.path, &dest, true, &inside)?
            } else {
                extract_package_streaming(p, &blob.path, &dest, &inside)?
            };
            Ok((p.path.clone(), n, bins))
        })
        .collect();
    let mut total = 0;
    let mut bins: HashMap<String, Bins> = HashMap::new();
    for w in written {
        let (path, n, b) = w?;
        total += n;
        if !b.is_empty() {
            bins.insert(path, b);
        }
    }
    for l in &plan.links {
        let dest = root.join(&l.path);
        if let Some(parent) = dest.parent() {
            inside.dir(parent)?;
        }
        let _ = std::fs::remove_file(&dest);
        std::os::unix::fs::symlink(&l.target, &dest).with_context(|| format!("linking {}", l.path))?;
    }
    for (dir, entries) in bin_links(plan, &bins) {
        let bin_dir = root.join(&dir).join(".bin");
        inside.dir(&bin_dir)?;
        for (name, target) in entries {
            let dest = bin_dir.join(&name);
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&target, &dest)?;
            let resolved = bin_dir.join(&target);
            if let Ok(real) = std::fs::canonicalize(&resolved)
                && real.starts_with(&inside.root)
                && let Ok(meta) = std::fs::metadata(&real)
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o111);
                let _ = std::fs::set_permissions(&real, perm);
            }
        }
    }
    acropolis_events::add_written(total);
    Ok(total)
}

fn open_tarball(blob: &Path) -> Result<TarReader<BufReader<Box<dyn Read>>>> {
    let file = std::fs::File::open(blob)?;
    let mut raw = BufReader::with_capacity(128 * 1024, file);
    let mut magic = [0u8; 2];
    let n = raw.read(&mut magic)?;
    let chained = std::io::Cursor::new(magic[..n].to_vec()).chain(raw);
    let reader: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(chained))
    } else {
        Box::new(chained)
    };
    Ok(TarReader::new(BufReader::with_capacity(128 * 1024, reader)))
}

fn extract_package_streaming(p: &InstallPackage, blob: &Path, dest: &Path, inside: &Inside) -> Result<(u64, Bins)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut tr = open_tarball(blob).with_context(|| format!("extracting {}", p.path))?;
    inside.dir(dest)?;
    let mut made: HashSet<std::path::PathBuf> = HashSet::new();
    made.insert(dest.to_path_buf());
    let mut files: Vec<TarEntry> = Vec::new();
    let mut package_json: Option<Vec<u8>> = None;
    let mut total = 0u64;
    while let Some(e) = tr.next_entry().with_context(|| format!("extracting {}", p.path))? {
        if !matches!(e.kind, Kind::File | Kind::Dir) {
            continue;
        }
        let Some((_, rest)) = e.path.split_once('/') else {
            continue;
        };
        let Some(rel) = clean_rel(rest) else { continue };
        let path = dest.join(&rel);
        if e.kind == Kind::Dir {
            if made.insert(path.clone()) {
                inside.dir(&path)?;
            }
            continue;
        }
        if let Some(parent) = path.parent()
            && made.insert(parent.to_path_buf())
        {
            inside.dir(parent)?;
        }
        let mode = normalize_mode(e.mode, Kind::File);
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(mode)
            .open(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        if rel == "package.json" {
            let data = tr.read_data()?;
            std::io::Write::write_all(&mut f, &data)?;
            total += data.len() as u64;
            package_json = Some(data);
        } else {
            total += std::io::copy(&mut tr.data(), &mut f)?;
        }
        files.push(TarEntry {
            rel,
            kind: Kind::File,
            mode,
            data: Vec::new(),
        });
    }
    let bins = match &p.bins {
        Some(b) => b.clone(),
        None => package_json
            .as_deref()
            .map(|d| bins_from_package_json(&p.name, d, &files))
            .unwrap_or_default(),
    };
    for (_, t) in &bins {
        if let Some(t) = clean_rel(t) {
            use std::os::unix::fs::PermissionsExt;
            let path = dest.join(&t);
            if inside.contains(&path)
                && let Ok(meta) = std::fs::symlink_metadata(&path)
                && meta.is_file()
            {
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
    Ok((total, bins))
}

fn extract_package_with(
    p: &InstallPackage,
    blob: &Path,
    dest: &Path,
    types_only: bool,
    inside: &Inside,
) -> Result<(u64, Bins)> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut entries = read_tarball(blob).with_context(|| format!("extracting {}", p.path))?;
    let bins = package_bins(p, &entries);
    if types_only && type_files_only(&entries, &bins) {
        entries.retain(|e| {
            e.kind == Kind::Dir || e.rel == "package.json" || TYPE_FILES.iter().any(|x| e.rel.ends_with(x))
        });
    }
    let execs: HashSet<String> = bins.iter().filter_map(|(_, t)| clean_rel(t)).collect();
    inside.dir(dest)?;
    let mut made: HashSet<std::path::PathBuf> = HashSet::new();
    made.insert(dest.to_path_buf());
    let mut total = 0u64;
    for e in &entries {
        let path = dest.join(&e.rel);
        if e.kind == Kind::Dir {
            if made.insert(path.clone()) {
                inside.dir(&path)?;
            }
            continue;
        }
        if let Some(parent) = path.parent()
            && made.insert(parent.to_path_buf())
        {
            inside.dir(parent)?;
        }
        let mode = if execs.contains(&e.rel) {
            0o755
        } else {
            normalize_mode(e.mode, Kind::File)
        };
        let _ = std::fs::remove_file(&path);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(mode)
            .open(&path)?;
        f.write_all(&e.data)?;
        total += e.data.len() as u64;
    }
    Ok((total, bins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_links() {
        assert_eq!(
            relative_link(Path::new("node_modules/a"), Path::new("packages/a")),
            "../packages/a"
        );
        assert_eq!(
            relative_link(Path::new("node_modules/@s/a"), Path::new("packages/a")),
            "../../packages/a"
        );
        assert_eq!(
            relative_link(
                Path::new("node_modules/.bin/vite"),
                Path::new("node_modules/vite/bin/vite.js")
            ),
            "../vite/bin/vite.js"
        );
    }

    #[test]
    fn platform_filters() {
        let p = Platform::default();
        let mut e = LockEntry {
            os: Some(vec!["darwin".into()]),
            ..Default::default()
        };
        assert!(!platform_ok(&e, &p));
        e.os = Some(vec!["!win32".into()]);
        assert!(platform_ok(&e, &p));
    }

    #[test]
    fn git_urls() {
        assert_eq!(
            git_tarball_url(
                "git+ssh://git@github.com/iloveitaly/rehype-remove-images.git#6307a5d2b29f4b96f08fb4f62f6c2badf012cef2"
            )
            .unwrap(),
            "https://codeload.github.com/iloveitaly/rehype-remove-images/tar.gz/6307a5d2b29f4b96f08fb4f62f6c2badf012cef2"
        );
        assert_eq!(
            git_tarball_url("github:a/b#abcdef1").unwrap(),
            "https://codeload.github.com/a/b/tar.gz/abcdef1"
        );
        assert!(git_tarball_url("git+https://gitlab.com/a/b.git#abcdef1").is_none());
    }

    #[test]
    fn platform_from_names() {
        let p = Platform {
            os: "linux".into(),
            cpu: "x64".into(),
            libc: "glibc".into(),
        };
        let ok = |n: &str| platform_matches_named(n, &None, &None, &None, &p);
        for n in [
            "@img/sharp-linux-x64",
            "@img/sharp-libvips-linux-x64",
            "@esbuild/linux-x64",
            "@next/swc-linux-x64-gnu",
            "lightningcss-linux-x64-gnu",
            "sharp",
            "react",
            "linux-utils",
            "@rollup/rollup-linux-x64-gnu",
        ] {
            assert!(ok(n), "{n}");
        }
        for n in [
            "@img/sharp-darwin-arm64",
            "@img/sharp-libvips-linuxmusl-x64",
            "@img/sharp-win32-ia32",
            "@img/sharp-wasm32",
            "@img/sharp-webcontainers-wasm32",
            "@img/sharp-freebsd-wasm32",
            "@esbuild/linux-arm64",
            "@next/swc-linux-x64-musl",
            "@esbuild/android-arm",
        ] {
            assert!(!ok(n), "{n}");
        }
    }

    #[test]
    fn unsafe_paths() {
        assert!(safe_rel_path("node_modules/a").is_ok());
        assert!(safe_rel_path("node_modules/.pnpm/a@1/node_modules/a").is_ok());
        assert!(safe_rel_path("node_modules/../../etc/cron.d/x").is_err());
        assert!(safe_rel_path("/etc/passwd").is_err());
        assert!(safe_rel_path("node_modules/a\0b").is_err());
    }

    #[test]
    fn extraction_stays_inside_root() {
        let base = std::env::temp_dir().join(format!("acropolis-inside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
        let inside = Inside::new(&root).unwrap();
        assert!(inside.dir(&root.join("node_modules/a")).is_ok());
        assert!(inside.dir(&root.join("evil/node_modules/a")).is_err());
        assert!(!outside.join("node_modules").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn joins() {
        assert_eq!(normalize_join(Path::new("node_modules"), "../packages/a"), "packages/a");
    }
}
