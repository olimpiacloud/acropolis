use acropolis_fetch::Fetcher;
use acropolis_store::{Algo, Integrity};
use acropolis_toolchain::{Installed, extract_tar_filtered};
use anyhow::{Context, Result, anyhow, bail};
use futures::future::try_join_all;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_RUST: &str = "stable";

pub fn host_triple() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-gnu",
        _ => "x86_64-unknown-linux-gnu",
    }
}

#[derive(Debug, Deserialize)]
struct ChannelManifest {
    date: String,
    pkg: BTreeMap<String, PkgEntry>,
}

#[derive(Debug, Deserialize)]
struct PkgEntry {
    version: String,
    #[serde(default)]
    target: BTreeMap<String, TargetEntry>,
}

#[derive(Debug, Deserialize, Clone)]
struct TargetEntry {
    available: bool,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    xz_url: Option<String>,
    #[serde(default)]
    xz_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Component {
    pub name: String,
    pub url: String,
    pub sha256: Integrity,
    pub xz: bool,
}

#[derive(Debug, Clone)]
pub struct RustRelease {
    pub version: String,
    pub date: String,
    pub components: Vec<Component>,
}

pub fn channel_name(spec: &str) -> String {
    let s = spec.trim();
    if s.is_empty() || s == "latest" {
        DEFAULT_RUST.to_string()
    } else {
        s.to_string()
    }
}

pub async fn resolve(fetcher: &Fetcher, spec: &str, targets: &[String]) -> Result<RustRelease> {
    let channel = channel_name(spec);
    let url = if let Some(date) = channel.strip_prefix("nightly-") {
        format!("https://static.rust-lang.org/dist/{date}/channel-rust-nightly.toml")
    } else if let Some(date) = channel.strip_prefix("beta-") {
        format!("https://static.rust-lang.org/dist/{date}/channel-rust-beta.toml")
    } else {
        format!("https://static.rust-lang.org/dist/channel-rust-{channel}.toml")
    };
    let bytes = fetcher
        .bytes(&url)
        .await
        .with_context(|| format!("fetching Rust channel {channel}"))?;
    let text = String::from_utf8(bytes.to_vec()).context("channel manifest is not UTF-8")?;
    let manifest: ChannelManifest = toml::from_str(&text).context("parsing channel manifest")?;
    let host = host_triple();
    let mut components = Vec::new();
    let mut want: Vec<(String, String)> = vec![
        ("rustc".into(), host.into()),
        ("cargo".into(), host.into()),
        ("rust-std".into(), host.into()),
    ];
    for t in targets {
        if t != host {
            want.push(("rust-std".into(), t.clone()));
        }
    }
    let mut version = String::new();
    for (pkg, target) in want {
        let entry = manifest
            .pkg
            .get(&pkg)
            .ok_or_else(|| anyhow!("channel {channel} has no {pkg}"))?;
        if pkg == "rustc" {
            version = entry.version.split_whitespace().next().unwrap_or("").to_string();
        }
        let t = entry
            .target
            .get(&target)
            .filter(|t| t.available)
            .ok_or_else(|| anyhow!("{pkg} is not available for {target} in {channel}"))?;
        let prefer_gz = std::env::var("ACROPOLIS_RUST_COMPRESSION")
            .map(|v| v == "gz")
            .unwrap_or(true);
        let (url, hash, xz) = match (&t.xz_url, &t.xz_hash, &t.url, &t.hash) {
            (_, _, Some(u), Some(h)) if prefer_gz => (u.clone(), h.clone(), false),
            (Some(u), Some(h), _, _) => (u.clone(), h.clone(), true),
            (_, _, Some(u), Some(h)) => (u.clone(), h.clone(), false),
            _ => bail!("{pkg} for {target} has no download URL"),
        };
        components.push(Component {
            name: format!("{pkg}-{target}"),
            url,
            sha256: Integrity::parse_hex(Algo::Sha256, &hash)?,
            xz,
        });
    }
    Ok(RustRelease {
        version,
        date: manifest.date,
        components,
    })
}

pub async fn install(fetcher: &Fetcher, release: &RustRelease, dest: &Path) -> Result<Installed> {
    let futs = release.components.iter().map(|c| {
        let dest = dest.to_path_buf();
        let xz = c.xz;
        async move {
            let headers = HeaderMap::new();
            let what = c.url.rsplit('/').next().unwrap_or(&c.url).to_string();
            let (_, blob) = fetcher
                .blob_streaming(&what, &c.url, &headers, Some(c.sha256.clone()), move |r| {
                    let mut reader: Box<dyn std::io::Read> = if xz {
                        Box::new(liblzma::read::XzDecoder::new(r))
                    } else {
                        Box::new(flate2::read::GzDecoder::new(r))
                    };
                    extract_tar_filtered(&mut reader, &dest, 2, &|rel: &str| {
                        rel != "manifest.in" && !rel.starts_with("share/doc") && !rel.starts_with("share/man")
                    })
                })
                .await
                .with_context(|| format!("installing {}", c.name))?;
            Ok::<_, anyhow::Error>(blob)
        }
    });
    try_join_all(futs).await?;
    Ok(Installed {
        name: "rust".into(),
        version: release.version.clone(),
        root: dest.to_path_buf(),
        bin_dir: dest.join("bin"),
        archive: None,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct LockPackage {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CargoLock {
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default, rename = "package")]
    pub packages: Vec<LockPackage>,
}

pub fn parse_lock(text: &str) -> Result<CargoLock> {
    toml::from_str(text).context("parsing Cargo.lock")
}

/// `major.minor.patch` of a `rust-version` field (`1.94` means `1.94.0`).
pub fn parse_rust_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().split('.').map(|p| p.parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next().flatten().unwrap_or(0);
    let patch = it.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Highest `rust-version` declared by the locked crates.io packages vendored in `vendor_dir` (see `vendor`).
pub fn max_rust_version(lock: &CargoLock, vendor_dir: &Path) -> Option<(u64, u64, u64)> {
    lock.packages
        .iter()
        .filter(|p| p.source.is_some())
        .filter_map(|p| {
            let text = std::fs::read_to_string(vendor_dir.join(format!("{}-{}", p.name, p.version)).join("Cargo.toml"))
                .ok()?;
            let manifest: toml::Value = toml::from_str(&text).ok()?;
            parse_rust_version(manifest.get("package")?.get("rust-version")?.as_str()?)
        })
        .max()
}

/// Whether the workspace links the system libsqlite3. Cargo.lock lists `libsqlite3-sys` for every sqlx app (the lock
/// ignores features), but sqlx's `sqlite` feature and the `bundled*` features of rusqlite/libsqlite3-sys compile
/// SQLite from source. The system library is needed for sqlx with `sqlite-unbundled`, or rusqlite/libsqlite3-sys
/// without a `bundled*` feature, in any dependency table or `[features]` entry of the workspace manifests.
pub fn sqlite_needs_system_lib(dir: &Path) -> bool {
    let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for manifest in workspace_manifests(dir) {
        let mut packages: BTreeMap<String, String> = BTreeMap::new();
        let mut tables: Vec<&toml::Value> = Vec::new();
        for root in [Some(&manifest), manifest.get("workspace")].into_iter().flatten() {
            for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
                tables.extend(root.get(kind));
            }
        }
        if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
            for t in targets.values() {
                for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
                    tables.extend(t.get(kind));
                }
            }
        }
        for (key, dep) in tables.iter().filter_map(|t| t.as_table()).flatten() {
            let package = dep.get("package").and_then(|p| p.as_str()).unwrap_or(key).to_string();
            let list = features.entry(package.clone()).or_default();
            for f in dep.get("features").and_then(|f| f.as_array()).into_iter().flatten() {
                list.extend(f.as_str().map(str::to_string));
            }
            packages.insert(key.clone(), package);
        }
        for value in manifest
            .get("features")
            .and_then(|f| f.as_table())
            .into_iter()
            .flat_map(|t| t.values())
        {
            for f in value.as_array().into_iter().flatten().filter_map(|f| f.as_str()) {
                if let Some((dep, feature)) = f.split_once('/') {
                    let dep = dep.trim_end_matches('?');
                    let package = packages.get(dep).cloned().unwrap_or_else(|| dep.to_string());
                    features.entry(package).or_default().push(feature.to_string());
                }
            }
        }
    }
    let has = |package: &str, feature: &str| features.get(package).is_some_and(|f| f.iter().any(|x| x == feature));
    has("sqlx", "sqlite-unbundled")
        || ["rusqlite", "libsqlite3-sys"].iter().any(|p| {
            features
                .get(*p)
                .is_some_and(|f| !f.iter().any(|x| x.starts_with("bundled")))
        })
}

/// Every Cargo.toml in the app up to three levels deep (workspace members), skipping build outputs and symlinks.
fn workspace_manifests(dir: &Path) -> Vec<toml::Value> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<toml::Value>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let name = e.file_name();
            if ft.is_file() && name == "Cargo.toml" {
                if let Some(v) = std::fs::read_to_string(e.path())
                    .ok()
                    .and_then(|t| toml::from_str(&t).ok())
                {
                    out.push(v);
                }
            } else if ft.is_dir()
                && depth > 0
                && !matches!(name.to_str(), Some("target" | "vendor" | "node_modules" | ".git"))
            {
                walk(&e.path(), depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 3, &mut out);
    out
}

pub const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";
pub const CRATES_IO_SPARSE: &str = "sparse+https://index.crates.io/";

pub struct VendorStats {
    pub crates: usize,
    pub bytes: u64,
}

pub async fn vendor(fetcher: &Fetcher, lock: &CargoLock, vendor_dir: &Path) -> Result<VendorStats> {
    let mut regs = Vec::new();
    for p in &lock.packages {
        match p.source.as_deref() {
            None => continue,
            Some(s) if s == CRATES_IO || s == CRATES_IO_SPARSE => regs.push(p.clone()),
            Some(s) if s.starts_with("git+") => bail!("git dependency {} ({s}) is not supported yet", p.name),
            Some(s) => bail!("unsupported crate source {s} for {}", p.name),
        }
        let name_ok = !p.name.is_empty()
            && p.name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let version_ok = !p.version.is_empty()
            && p.version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b));
        if !name_ok || !version_ok {
            bail!(
                "invalid crate name or version in Cargo.lock: {:?} {:?}",
                p.name,
                p.version
            );
        }
    }
    std::fs::create_dir_all(vendor_dir)?;
    let futs = regs.iter().map(|p| {
        let dir = vendor_dir.join(format!("{}-{}", p.name, p.version));
        async move {
            let checksum = p
                .checksum
                .clone()
                .ok_or_else(|| anyhow!("{} {} has no checksum in Cargo.lock", p.name, p.version))?;
            let marker = format!("{{\"files\":{{}},\"package\":\"{checksum}\"}}");
            if std::fs::read_to_string(dir.join(".cargo-checksum.json"))
                .map(|m| m == marker)
                .unwrap_or(false)
            {
                return Ok::<u64, anyhow::Error>(0);
            }
            let _ = std::fs::remove_dir_all(&dir);
            let expected = Integrity::parse_hex(Algo::Sha256, &checksum)?;
            let url = format!("https://static.crates.io/crates/{0}/{0}-{1}.crate", p.name, p.version);
            let headers = HeaderMap::new();
            let d2 = dir.clone();
            let (stats, _) = fetcher
                .blob_streaming(
                    &format!("{}-{}.crate", p.name, p.version),
                    &url,
                    &headers,
                    Some(expected),
                    move |r| {
                        let mut gz = Capped {
                            inner: flate2::read::GzDecoder::new(r),
                            left: MAX_CRATE_UNPACKED,
                        };
                        extract_tar_filtered(&mut gz, &d2, 1, &|rel| rel != ".cargo-checksum.json")
                    },
                )
                .await?;
            std::fs::write(dir.join(".cargo-checksum.json"), &marker)?;
            Ok::<u64, anyhow::Error>(stats.bytes)
        }
    });
    let sizes = try_join_all(futs).await?;
    Ok(VendorStats {
        crates: sizes.len(),
        bytes: sizes.iter().sum(),
    })
}

/// Cargo's own limit for an unpacked crate (CVE-2022-36114).
const MAX_CRATE_UNPACKED: u64 = 512 << 20;

struct Capped<R> {
    inner: R,
    left: u64,
}

impl<R: std::io::Read> std::io::Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.left = self
            .left
            .checked_sub(n as u64)
            .ok_or_else(|| std::io::Error::other("crate unpacks to more than 512 MiB"))?;
        Ok(n)
    }
}

pub fn cargo_config(vendor_dir: &Path) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"acropolis-vendored\"\n\n[source.acropolis-vendored]\ndirectory = \"{}\"\n\n[net]\noffline = true\n",
        vendor_dir.display()
    )
}

#[derive(Debug, Clone, Default)]
pub struct RustProject {
    pub package_name: Option<String>,
    pub bins: Vec<String>,
    pub workspace_members: Vec<String>,
    pub rust_version: Option<String>,
    pub default_run: Option<String>,
    pub edition: Option<String>,
}

/// Repo files may be symlinks; one pointing at /proc/self/environ or a host secret must not reach logs or errors.
fn read_app_file(dir: &Path, rel: &str) -> Option<String> {
    let real = std::fs::canonicalize(dir.join(rel)).ok()?;
    if !real.starts_with(std::fs::canonicalize(dir).ok()?) || !real.is_file() {
        return None;
    }
    std::fs::read_to_string(real).ok()
}

pub fn read_project(dir: &Path) -> Result<RustProject> {
    let text = read_app_file(dir, "Cargo.toml")
        .ok_or_else(|| anyhow!("reading Cargo.toml: not a regular file inside the app"))?;
    let v: toml::Value = toml::from_str(&text).context("parsing Cargo.toml")?;
    let mut p = RustProject::default();
    if let Some(pkg) = v.get("package") {
        p.package_name = pkg.get("name").and_then(|n| n.as_str()).map(|s| s.to_string());
        p.rust_version = pkg.get("rust-version").and_then(|n| n.as_str()).map(|s| s.to_string());
        p.default_run = pkg.get("default-run").and_then(|n| n.as_str()).map(|s| s.to_string());
        p.edition = pkg.get("edition").and_then(|n| n.as_str()).map(|s| s.to_string());
    }
    if let Some(bins) = v.get("bin").and_then(|b| b.as_array()) {
        for b in bins {
            if let Some(n) = b.get("name").and_then(|n| n.as_str()) {
                p.bins.push(n.to_string());
            }
        }
    }
    if let Some(members) = v
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        p.workspace_members = members
            .iter()
            .filter_map(|m| m.as_str().map(|s| s.to_string()))
            .collect();
    }
    Ok(p)
}

pub fn toolchain_file(dir: &Path) -> Option<String> {
    if let Some(text) = read_app_file(dir, "rust-toolchain.toml")
        && let Ok(v) = toml::from_str::<toml::Value>(&text)
        && let Some(c) = v
            .get("toolchain")
            .and_then(|t| t.get("channel"))
            .and_then(|c| c.as_str())
    {
        return Some(c.to_string());
    }
    if let Some(text) = read_app_file(dir, "rust-toolchain") {
        let t = text.trim();
        if t.starts_with('[') {
            if let Ok(v) = toml::from_str::<toml::Value>(t)
                && let Some(c) = v
                    .get("toolchain")
                    .and_then(|t| t.get("channel"))
                    .and_then(|c| c.as_str())
            {
                return Some(c.to_string());
            }
        } else if !t.is_empty() {
            return Some(t.lines().next().unwrap_or("").trim().to_string());
        }
    }
    None
}

pub fn host_glibc() -> Option<(u32, u32)> {
    let out = std::process::Command::new("ldd").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?;
    let ver = first.split_whitespace().last()?;
    let mut it = ver.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

pub fn runtime_base_for_glibc(glibc: Option<(u32, u32)>) -> Result<&'static str> {
    match glibc {
        Some(v) if v <= (2, 36) => Ok("gcr.io/distroless/cc-debian12"),
        Some(v) if v <= (2, 41) => Ok("gcr.io/distroless/cc-debian13"),
        Some((a, b)) => {
            bail!("host glibc {a}.{b} is newer than any supported runtime base; build for the musl target instead")
        }
        None => Ok("gcr.io/distroless/cc-debian12"),
    }
}

pub fn target_dir(dir: &Path) -> PathBuf {
    dir.join("target")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_and_config() {
        let lock = parse_lock("version = 4\n[[package]]\nname = \"a\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"00\"\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n").unwrap();
        assert_eq!(lock.packages.len(), 2);
        assert!(cargo_config(Path::new("/v")).contains("directory = \"/v\""));
        assert_eq!(
            runtime_base_for_glibc(Some((2, 39))).unwrap(),
            "gcr.io/distroless/cc-debian13"
        );
    }

    #[test]
    fn vendor_rejects_lockfile_paths_outside_vendor_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim-1");
        std::fs::create_dir_all(&victim).unwrap();
        let store = std::sync::Arc::new(acropolis_store::Store::open(tmp.path().join("store")).unwrap());
        let fetcher = Fetcher::new(store, 1).unwrap();
        let lock = parse_lock(&format!(
            "[[package]]\nname = \"../victim\"\nversion = \"1\"\nsource = \"{CRATES_IO}\"\nchecksum = \"{}\"\n",
            "0".repeat(64)
        ))
        .unwrap();
        assert!(futures::executor::block_on(vendor(&fetcher, &lock, &tmp.path().join("vendor"))).is_err());
        assert!(victim.exists());
    }

    #[test]
    fn toolchain_file_ignores_symlinks_outside_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(tmp.path().join("secret"), "TOKEN=hunter2\n").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret"), app.join("rust-toolchain")).unwrap();
        std::os::unix::fs::symlink("../secret", app.join("Cargo.toml")).unwrap();
        assert_eq!(toolchain_file(&app), None);
        assert!(!read_project(&app).unwrap_err().to_string().contains("hunter2"));
        std::fs::remove_file(app.join("rust-toolchain")).unwrap();
        std::fs::write(app.join("real"), "1.80.0\n").unwrap();
        std::os::unix::fs::symlink("real", app.join("rust-toolchain")).unwrap();
        assert_eq!(toolchain_file(&app).as_deref(), Some("1.80.0"));
    }

    #[test]
    fn highest_rust_version_of_the_locked_crates() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = parse_lock(
            "[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
             [[package]]\nname = \"sqlx\"\nversion = \"0.9.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"old\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        for (dir, manifest) in [
            ("sqlx-0.9.0", "[package]\nname = \"sqlx\"\nrust-version = \"1.94\"\n"),
            (
                "serde-1.0.0",
                "[package]\nname = \"serde\"\nrust-version = \"1.61.0\"\n",
            ),
            ("old-1.0.0", "[package]\nname = \"old\"\n"),
            ("stale-9.9.9", "[package]\nname = \"stale\"\nrust-version = \"1.99\"\n"),
        ] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
            std::fs::write(tmp.path().join(dir).join("Cargo.toml"), manifest).unwrap();
        }
        assert_eq!(max_rust_version(&lock, tmp.path()), Some((1, 94, 0)));
        assert_eq!(parse_rust_version("1.85.1"), Some((1, 85, 1)));
        assert!(parse_rust_version("stable").is_none());
    }
}
