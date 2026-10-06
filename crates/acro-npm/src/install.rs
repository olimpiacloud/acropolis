use crate::lockfile::{LockEntry, PackageLock, bins_of, package_name_from_path};
use acro_fetch::Fetcher;
use acro_oci::tar::{Kind, TarReader, TarWriter, normalize_mode};
use acro_store::{Integrity, StoredBlob};
use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use futures::future::try_join_all;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::Path;

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
        Platform { os: "linux".into(), cpu: cpu.into(), libc: "glibc".into() }
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

pub fn platform_matches(os: &Option<Vec<String>>, cpu: &Option<Vec<String>>, libc: &Option<Vec<String>>, p: &Platform) -> bool {
    matches_list(os, &p.os) && matches_list(cpu, &p.cpu) && matches_list(libc, &p.libc)
}

fn platform_ok(e: &LockEntry, p: &Platform) -> bool {
    platform_matches(&e.os, &e.cpu, &e.libc, p)
}

pub fn parent_node_modules(path: &str) -> Option<(&str, &str)> {
    let idx = path.rfind("node_modules/")?;
    Some((&path[..idx + "node_modules".len()], &path[idx + "node_modules/".len()..]))
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
            if skipped.iter().any(|s| path.starts_with(&format!("{s}/"))) {
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
            if !platform_ok(e, &opts.platform) {
                skipped.push(path.clone());
                out.skipped_platform.push(path.clone());
                continue;
            }
            let name = e.name.clone().unwrap_or_else(|| package_name_from_path(path).to_string());
            if let Some((nm, _)) = parent_node_modules(path) {
                bin_dirs.entry(nm.to_string()).or_default().push(path.clone());
            }
            if e.link {
                let target = e.resolved.clone().ok_or_else(|| anyhow!("link {path} without resolved"))?;
                if let Some(t) = lock.packages.get(&target) {
                    let bins = bins_of(&name, &t.bin);
                    if !bins.is_empty() {
                        out.known_bins.insert(path.clone(), bins);
                    }
                }
                out.links.push(Link { path: path.clone(), target: relative_link(Path::new(path), Path::new(&target)) });
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
                    Source::Registry { url: resolved, integrity }
                } else if let Some(target) = resolved.strip_prefix("file:") {
                    out.links.push(Link { path: path.clone(), target: relative_link(Path::new(path), Path::new(target)) });
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
                    Source::Registry { url: default_registry_url(&name, &version), integrity }
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
        out.bin_dirs = bin_dirs.into_iter().map(|(dir, packages)| BinDir { dir, packages }).collect();
        Ok(out)
    }

    pub fn registry_packages(&self) -> impl Iterator<Item = &InstallPackage> {
        self.packages.iter().filter(|p| matches!(p.source, Source::Registry { .. }))
    }

    pub fn with_install_scripts(&self) -> Vec<&InstallPackage> {
        self.packages.iter().filter(|p| p.has_install_script).collect()
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
    } else if let Some(rest) = repo.strip_prefix("github.com/").or_else(|| repo.strip_prefix("github.com:")) {
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
                let t = git_tarball_url(url).ok_or_else(|| anyhow!("git dependency {} ({url}) is only supported for GitHub repositories pinned to a commit", p.path))?;
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
    let mut tr = TarReader::new(BufReader::with_capacity(128 * 1024, reader));
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
        let data = if e.kind == Kind::File { tr.read_data()? } else { Vec::new() };
        if let Some(&i) = seen.get(&rel) {
            out[i] = TarEntry { rel, kind: e.kind, mode: e.mode, data };
            continue;
        }
        seen.insert(rel.clone(), out.len());
        out.push(TarEntry { rel, kind: e.kind, mode: e.mode, data });
    }
    Ok(out)
}

pub fn bins_from_package_json(name: &str, data: &[u8], files: &[TarEntry]) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else { return Vec::new() };
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

fn package_bins(p: &InstallPackage, entries: &[TarEntry]) -> Vec<(String, String)> {
    match &p.bins {
        Some(b) => b.clone(),
        None => entries
            .iter()
            .find(|e| e.rel == "package.json")
            .map(|e| bins_from_package_json(&p.name, &e.data, entries))
            .unwrap_or_default(),
    }
}

fn package_fragment(prefix: &str, p: &InstallPackage, blob: &Path) -> Result<(Vec<u8>, Vec<(String, String)>)> {
    let entries = read_tarball(blob).with_context(|| format!("reading tarball of {}", p.path))?;
    let bins = package_bins(p, &entries);
    let execs: HashSet<String> = bins.iter().filter_map(|(_, t)| clean_rel(t)).collect();
    let cap: usize = entries.iter().map(|e| e.data.len() + 1024).sum();
    let mut tw = TarWriter::new(Vec::with_capacity(cap));
    let mut dirs: HashSet<String> = HashSet::new();
    let root = if prefix.is_empty() { p.path.clone() } else { format!("{prefix}/{}", p.path) };
    tw.dir(&root, 0o755)?;
    dirs.insert(root.clone());
    for e in &entries {
        let parts: Vec<&str> = e.rel.split('/').collect();
        let upto = if e.kind == Kind::Dir { parts.len() } else { parts.len() - 1 };
        let mut acc = root.clone();
        for part in &parts[..upto] {
            acc.push('/');
            acc.push_str(part);
            if dirs.insert(acc.clone()) {
                tw.dir(&acc, 0o755)?;
            }
        }
        if e.kind == Kind::File {
            let mode = if execs.contains(&e.rel) { 0o755 } else { normalize_mode(e.mode, Kind::File) };
            tw.file_bytes(&format!("{root}/{}", e.rel), mode, &e.data)?;
        }
    }
    Ok((take(tw), bins))
}

fn take(mut tw: TarWriter<Vec<u8>>) -> Vec<u8> {
    std::mem::take(tw.get_mut())
}

fn bin_links(plan: &InstallPlan, bins: &HashMap<String, Vec<(String, String)>>) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let link_targets: HashMap<&str, &str> = plan.links.iter().map(|l| (l.path.as_str(), l.target.as_str())).collect();
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
    let mut parts: Vec<String> = base.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
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

pub struct LayerFragments {
    pub fragments: Vec<Vec<u8>>,
}

pub fn node_modules_fragments(
    plan: &InstallPlan,
    tarballs: &Tarballs,
    prefix: &str,
    filter: impl Fn(&InstallPackage) -> bool + Sync,
) -> Result<LayerFragments> {
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
    let base = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
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
    let mut fragments = vec![take(head)];
    let bodies: Vec<Result<(String, Vec<u8>, Vec<(String, String)>)>> = pkgs
        .par_iter()
        .filter(|p| fetched(p))
        .map(|p| {
            let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
            let (frag, bins) = package_fragment(prefix, p, &blob.path)?;
            Ok((p.path.clone(), frag, bins))
        })
        .collect();
    let mut bins: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for b in bodies {
        let (path, frag, b) = b?;
        fragments.push(frag);
        if !b.is_empty() {
            bins.insert(path, b);
        }
    }
    let mut tail = TarWriter::new(Vec::new());
    for (dir, entries) in bin_links(plan, &bins) {
        tail.dir(&format!("{base}{dir}/.bin"), 0o755)?;
        for (name, target) in entries {
            tail.symlink(&format!("{base}{dir}/.bin/{name}"), &target)?;
        }
    }
    fragments.push(take(tail));
    Ok(LayerFragments { fragments })
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

pub fn materialize(plan: &InstallPlan, tarballs: &Tarballs, root: &Path) -> Result<u64> {
    let pkgs: Vec<&InstallPackage> = plan.packages.iter().collect();
    let written: Vec<Result<(String, u64, Vec<(String, String)>)>> = pkgs
        .par_iter()
        .filter(|p| fetched(p))
        .map(|p| {
            let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
            let (n, bins) = extract_package(p, &blob.path, &root.join(&p.path))?;
            Ok((p.path.clone(), n, bins))
        })
        .collect();
    let mut total = 0;
    let mut bins: HashMap<String, Vec<(String, String)>> = HashMap::new();
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
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&dest);
        std::os::unix::fs::symlink(&l.target, &dest).with_context(|| format!("linking {}", l.path))?;
    }
    for (dir, entries) in bin_links(plan, &bins) {
        let bin_dir = root.join(&dir).join(".bin");
        std::fs::create_dir_all(&bin_dir)?;
        for (name, target) in entries {
            let dest = bin_dir.join(&name);
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&target, &dest)?;
            let resolved = bin_dir.join(&target);
            if let Ok(meta) = std::fs::metadata(&resolved) {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o111);
                let _ = std::fs::set_permissions(&resolved, perm);
            }
        }
    }
    acro_events::add_written(total);
    Ok(total)
}

pub fn extract_package(p: &InstallPackage, blob: &Path, dest: &Path) -> Result<(u64, Vec<(String, String)>)> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let entries = read_tarball(blob).with_context(|| format!("extracting {}", p.path))?;
    let bins = package_bins(p, &entries);
    let execs: HashSet<String> = bins.iter().filter_map(|(_, t)| clean_rel(t)).collect();
    std::fs::create_dir_all(dest)?;
    let mut made: HashSet<std::path::PathBuf> = HashSet::new();
    made.insert(dest.to_path_buf());
    let mut total = 0u64;
    for e in &entries {
        let path = dest.join(&e.rel);
        if e.kind == Kind::Dir {
            if made.insert(path.clone()) {
                std::fs::create_dir_all(&path)?;
            }
            continue;
        }
        if let Some(parent) = path.parent()
            && made.insert(parent.to_path_buf())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mode = if execs.contains(&e.rel) { 0o755 } else { normalize_mode(e.mode, Kind::File) };
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&path)?;
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
        assert_eq!(relative_link(Path::new("node_modules/a"), Path::new("packages/a")), "../packages/a");
        assert_eq!(relative_link(Path::new("node_modules/@s/a"), Path::new("packages/a")), "../../packages/a");
        assert_eq!(
            relative_link(Path::new("node_modules/.bin/vite"), Path::new("node_modules/vite/bin/vite.js")),
            "../vite/bin/vite.js"
        );
    }

    #[test]
    fn platform_filters() {
        let p = Platform::default();
        let mut e = LockEntry { os: Some(vec!["darwin".into()]), ..Default::default() };
        assert!(!platform_ok(&e, &p));
        e.os = Some(vec!["!win32".into()]);
        assert!(platform_ok(&e, &p));
    }

    #[test]
    fn git_urls() {
        assert_eq!(
            git_tarball_url("git+ssh://git@github.com/iloveitaly/rehype-remove-images.git#6307a5d2b29f4b96f08fb4f62f6c2badf012cef2").unwrap(),
            "https://codeload.github.com/iloveitaly/rehype-remove-images/tar.gz/6307a5d2b29f4b96f08fb4f62f6c2badf012cef2"
        );
        assert_eq!(git_tarball_url("github:a/b#abcdef1").unwrap(), "https://codeload.github.com/a/b/tar.gz/abcdef1");
        assert!(git_tarball_url("git+https://gitlab.com/a/b.git#abcdef1").is_none());
    }

    #[test]
    fn joins() {
        assert_eq!(normalize_join(Path::new("node_modules"), "../packages/a"), "packages/a");
    }
}
