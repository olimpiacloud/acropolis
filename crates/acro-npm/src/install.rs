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
use std::path::{Path, PathBuf};

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
    pub bins: Vec<(String, String)>,
    pub has_install_script: bool,
    pub optional: bool,
    pub dev: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InstallPlan {
    pub packages: Vec<InstallPackage>,
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

fn platform_ok(e: &LockEntry, p: &Platform) -> bool {
    matches_list(&e.os, &p.os) && matches_list(&e.cpu, &p.cpu) && matches_list(&e.libc, &p.libc)
}

impl InstallPlan {
    pub fn from_lock(lock: &PackageLock, opts: &InstallOptions) -> Result<Self> {
        let mut out = InstallPlan::default();
        let mut skipped: Vec<String> = Vec::new();
        for (path, e) in &lock.packages {
            if path.is_empty() || !path.contains("node_modules/") {
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
            let source = if e.link {
                let target = e.resolved.clone().ok_or_else(|| anyhow!("link {path} without resolved"))?;
                Source::Link { target }
            } else if e.in_bundle {
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
                } else if resolved.starts_with("file:") {
                    Source::Link { target: resolved.trim_start_matches("file:").to_string() }
                } else if resolved.is_empty() {
                    let version = e.version.clone().unwrap_or_default();
                    if version.is_empty() {
                        bail!("{path} has no resolved URL or version in package-lock.json");
                    }
                    let url = default_registry_url(&name, &version);
                    let integrity = match &e.integrity {
                        Some(i) => Some(Integrity::parse_sri(i)?),
                        None => None,
                    };
                    Source::Registry { url, integrity }
                } else {
                    bail!("unsupported resolved URL for {path}: {resolved}");
                }
            };
            let mut bins = bins_of(&name, &e.bin);
            if let Source::Link { target } = &source
                && let Some(t) = lock.packages.get(target)
            {
                bins = bins_of(&name, &t.bin);
            }
            out.packages.push(InstallPackage {
                path: path.clone(),
                name,
                version: e.version.clone().unwrap_or_default(),
                source,
                bins,
                has_install_script: e.has_install_script,
                optional: e.optional || e.dev_optional,
                dev: e.dev,
            });
        }
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

pub async fn fetch_all(fetcher: &Fetcher, plan: &InstallPlan) -> Result<Tarballs> {
    let mut unique: BTreeMap<String, Option<Integrity>> = BTreeMap::new();
    for p in plan.registry_packages() {
        if let Source::Registry { url, integrity } = &p.source {
            unique.entry(url.clone()).or_insert_with(|| integrity.clone());
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
        _ => None,
    }
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

pub fn for_each_tarball_entry(
    blob: &Path,
    mut f: impl FnMut(&str, Kind, u32, u64, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let file = std::fs::File::open(blob)?;
    let gz = GzDecoder::new(BufReader::with_capacity(128 * 1024, file));
    let mut tr = TarReader::new(BufReader::with_capacity(128 * 1024, gz));
    while let Some(e) = tr.next_entry()? {
        if !matches!(e.kind, Kind::File | Kind::Dir) {
            continue;
        }
        let rel = match e.path.split_once('/') {
            Some((_, rest)) => rest,
            None => continue,
        };
        let Some(rel) = clean_rel(rel) else { continue };
        let size = e.size;
        let mut data = tr.data();
        f(&rel, e.kind, e.mode, size, &mut data)?;
    }
    Ok(())
}

fn bin_targets(p: &InstallPackage) -> HashSet<String> {
    p.bins.iter().filter_map(|(_, t)| clean_rel(t)).collect()
}

fn package_fragment(prefix: &str, p: &InstallPackage, blob: &Path) -> Result<Vec<u8>> {
    let mut tw = TarWriter::new(Vec::with_capacity(64 * 1024));
    let mut dirs: HashSet<String> = HashSet::new();
    let root = format!("{prefix}/{}", p.path);
    tw.dir(&root, 0o755)?;
    dirs.insert(root.clone());
    let execs = bin_targets(p);
    for_each_tarball_entry(blob, |rel, kind, mode, size, data| {
        let full = format!("{root}/{rel}");
        let mut parent = String::new();
        let mut acc = root.clone();
        let parts: Vec<&str> = rel.split('/').collect();
        let upto = if kind == Kind::Dir { parts.len() } else { parts.len() - 1 };
        for part in &parts[..upto] {
            acc.push('/');
            acc.push_str(part);
            if dirs.insert(acc.clone()) {
                tw.dir(&acc, 0o755)?;
            }
            parent = acc.clone();
        }
        let _ = parent;
        if kind == Kind::File {
            let mode = if execs.contains(rel) { 0o755 } else { normalize_mode(mode, Kind::File) };
            tw.file_reader(&full, mode, size, data)?;
        }
        Ok(())
    })
    .with_context(|| format!("reading tarball of {}", p.path))?;
    Ok(tw.finish_fragment())
}

trait FinishFragment {
    fn finish_fragment(self) -> Vec<u8>;
}

impl FinishFragment for TarWriter<Vec<u8>> {
    fn finish_fragment(mut self) -> Vec<u8> {
        std::mem::take(self.get_mut())
    }
}

fn bin_links(packages: &[&InstallPackage]) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for p in packages {
        if p.bins.is_empty() {
            continue;
        }
        let Some(idx) = p.path.rfind("node_modules/") else { continue };
        let nm = &p.path[..idx + "node_modules".len()];
        let pkg_rel = &p.path[idx + "node_modules/".len()..];
        for (name, target) in &p.bins {
            let Some(t) = clean_rel(target) else { continue };
            if name.contains('/') || name.is_empty() {
                continue;
            }
            out.entry(nm.to_string()).or_default().insert(name.clone(), format!("../{pkg_rel}/{t}"));
        }
    }
    out
}

fn ancestors_of(path: &str, prefix: &str, set: &mut BTreeSet<String>) {
    let mut acc = prefix.to_string();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        acc.push('/');
        acc.push_str(part);
        set.insert(acc.clone());
    }
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
    for comp in prefix.split('/').scan(String::new(), |acc, c| {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(c);
        Some(acc.clone())
    }) {
        head_dirs.insert(comp);
    }
    for p in &pkgs {
        ancestors_of(&p.path, prefix, &mut head_dirs);
    }
    let mut head = TarWriter::new(Vec::new());
    for d in &head_dirs {
        head.dir(d, 0o755)?;
    }
    for p in &pkgs {
        if let Source::Link { target } = &p.source {
            let from = Path::new(&p.path);
            let link = relative_link(from, Path::new(target));
            head.symlink(&format!("{prefix}/{}", p.path), &link)?;
        }
    }
    let mut fragments = vec![head.finish_fragment()];
    let bodies: Vec<Result<Vec<u8>>> = pkgs
        .par_iter()
        .filter(|p| matches!(p.source, Source::Registry { .. }))
        .map(|p| {
            let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
            package_fragment(prefix, p, &blob.path)
        })
        .collect();
    for b in bodies {
        fragments.push(b?);
    }
    let mut tail = TarWriter::new(Vec::new());
    for (nm, bins) in bin_links(&pkgs) {
        tail.dir(&format!("{prefix}/{nm}/.bin"), 0o755)?;
        for (name, target) in bins {
            tail.symlink(&format!("{prefix}/{nm}/.bin/{name}"), &target)?;
        }
    }
    fragments.push(tail.finish_fragment());
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
    let written: Vec<Result<u64>> = pkgs
        .par_iter()
        .filter(|p| matches!(p.source, Source::Registry { .. }))
        .map(|p| {
            let blob = blob_for(tarballs, p).ok_or_else(|| anyhow!("missing tarball for {}", p.path))?;
            extract_package(p, &blob.path, &root.join(&p.path))
        })
        .collect();
    let mut total = 0;
    for w in written {
        total += w?;
    }
    for p in &pkgs {
        if let Source::Link { target } = &p.source {
            let dest = root.join(&p.path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let link = relative_link(Path::new(&p.path), Path::new(target));
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&link, &dest).with_context(|| format!("linking {}", p.path))?;
        }
    }
    for (nm, bins) in bin_links(&pkgs) {
        let dir = root.join(&nm).join(".bin");
        std::fs::create_dir_all(&dir)?;
        for (name, target) in bins {
            let dest = dir.join(&name);
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(&target, &dest)?;
            let resolved = dir.join(&target);
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

pub fn extract_package(p: &InstallPackage, blob: &Path, dest: &Path) -> Result<u64> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dest)?;
    let execs = bin_targets(p);
    let mut made: HashSet<PathBuf> = HashSet::new();
    made.insert(dest.to_path_buf());
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    for_each_tarball_entry(blob, |rel, kind, mode, size, data| {
        let path = dest.join(rel);
        if kind == Kind::Dir {
            if made.insert(path.clone()) {
                std::fs::create_dir_all(&path)?;
            }
            return Ok(());
        }
        if let Some(parent) = path.parent()
            && made.insert(parent.to_path_buf())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mode = if execs.contains(rel) { 0o755 } else { normalize_mode(mode, Kind::File) };
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&path)?;
        let mut remaining = size;
        while remaining > 0 {
            let want = remaining.min(buf.len() as u64) as usize;
            let n = data.read(&mut buf[..want])?;
            if n == 0 {
                bail!("truncated entry {rel}");
            }
            f.write_all(&buf[..n])?;
            remaining -= n as u64;
        }
        total += size;
        Ok(())
    })
    .with_context(|| format!("extracting {}", p.path))?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_links() {
        assert_eq!(relative_link(Path::new("node_modules/a"), Path::new("packages/a")), "../packages/a");
        assert_eq!(
            relative_link(Path::new("node_modules/@s/a"), Path::new("packages/a")),
            "../../packages/a"
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
}
