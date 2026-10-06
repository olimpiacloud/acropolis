use acro_fetch::Fetcher;
use acro_store::{Algo, Integrity};
use acro_toolchain::{Installed, extract_tar_filtered};
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
    if s.is_empty() || s == "latest" { DEFAULT_RUST.to_string() } else { s.to_string() }
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
    let bytes = fetcher.bytes(&url).await.with_context(|| format!("fetching Rust channel {channel}"))?;
    let text = String::from_utf8(bytes.to_vec()).context("channel manifest is not UTF-8")?;
    let manifest: ChannelManifest = toml::from_str(&text).context("parsing channel manifest")?;
    let host = host_triple();
    let mut components = Vec::new();
    let mut want: Vec<(String, String)> =
        vec![("rustc".into(), host.into()), ("cargo".into(), host.into()), ("rust-std".into(), host.into())];
    for t in targets {
        if t != host {
            want.push(("rust-std".into(), t.clone()));
        }
    }
    let mut version = String::new();
    for (pkg, target) in want {
        let entry = manifest.pkg.get(&pkg).ok_or_else(|| anyhow!("channel {channel} has no {pkg}"))?;
        if pkg == "rustc" {
            version = entry.version.split_whitespace().next().unwrap_or("").to_string();
        }
        let t = entry
            .target
            .get(&target)
            .filter(|t| t.available)
            .ok_or_else(|| anyhow!("{pkg} is not available for {target} in {channel}"))?;
        let prefer_gz = std::env::var("ACRO_RUST_COMPRESSION").map(|v| v == "gz").unwrap_or(true);
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
    Ok(RustRelease { version, date: manifest.date, components })
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
    }
    std::fs::create_dir_all(vendor_dir)?;
    let futs = regs.iter().map(|p| {
        let dir = vendor_dir.join(format!("{}-{}", p.name, p.version));
        async move {
            let checksum = p.checksum.clone().ok_or_else(|| anyhow!("{} {} has no checksum in Cargo.lock", p.name, p.version))?;
            let expected = Integrity::parse_hex(Algo::Sha256, &checksum)?;
            let url = format!("https://static.crates.io/crates/{0}/{0}-{1}.crate", p.name, p.version);
            let headers = HeaderMap::new();
            let d2 = dir.clone();
            let (stats, _) = fetcher
                .blob_streaming(&format!("{}-{}.crate", p.name, p.version), &url, &headers, Some(expected), move |r| {
                    let mut gz = flate2::read::GzDecoder::new(r);
                    extract_tar_filtered(&mut gz, &d2, 1, &|_| true)
                })
                .await?;
            std::fs::write(dir.join(".cargo-checksum.json"), format!("{{\"files\":{{}},\"package\":\"{checksum}\"}}"))?;
            Ok::<u64, anyhow::Error>(stats.bytes)
        }
    });
    let sizes = try_join_all(futs).await?;
    Ok(VendorStats { crates: sizes.len(), bytes: sizes.iter().sum() })
}

pub fn cargo_config(vendor_dir: &Path) -> String {
    format!(
        "[source.crates-io]\nreplace-with = \"acro-vendored\"\n\n[source.acro-vendored]\ndirectory = \"{}\"\n\n[net]\noffline = true\n",
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

pub fn read_project(dir: &Path) -> Result<RustProject> {
    let text = std::fs::read_to_string(dir.join("Cargo.toml")).context("reading Cargo.toml")?;
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
    if let Some(members) = v.get("workspace").and_then(|w| w.get("members")).and_then(|m| m.as_array()) {
        p.workspace_members = members.iter().filter_map(|m| m.as_str().map(|s| s.to_string())).collect();
    }
    Ok(p)
}

pub fn toolchain_file(dir: &Path) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(dir.join("rust-toolchain.toml")) {
        if let Ok(v) = toml::from_str::<toml::Value>(&text)
            && let Some(c) = v.get("toolchain").and_then(|t| t.get("channel")).and_then(|c| c.as_str())
        {
            return Some(c.to_string());
        }
    }
    if let Ok(text) = std::fs::read_to_string(dir.join("rust-toolchain")) {
        let t = text.trim();
        if t.starts_with('[') {
            if let Ok(v) = toml::from_str::<toml::Value>(t)
                && let Some(c) = v.get("toolchain").and_then(|t| t.get("channel")).and_then(|c| c.as_str())
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
        Some((a, b)) => bail!("host glibc {a}.{b} is newer than any supported runtime base; build for the musl target instead"),
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
        assert_eq!(runtime_base_for_glibc(Some((2, 39))).unwrap(), "gcr.io/distroless/cc-debian13");
    }
}
