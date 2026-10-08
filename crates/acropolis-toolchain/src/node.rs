use crate::semver_range::{best_match, is_exact};
use crate::{Installed, extract_tar_filtered, parse_shasums, verify};
use acropolis_fetch::Fetcher;
use anyhow::{Context, Result, anyhow};
use reqwest::header::HeaderMap;
use semver::Version;
use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_NODE: &str = "22";
pub const DIST: &str = "https://nodejs.org/dist";

#[derive(Deserialize)]
struct IndexEntry {
    version: String,
    #[serde(default)]
    lts: serde_json::Value,
}

pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    }
}

pub async fn resolve(fetcher: &Fetcher, spec: &str) -> Result<String> {
    let spec = spec.trim();
    let spec = if spec.is_empty() { DEFAULT_NODE } else { spec };
    if is_exact(spec) {
        return Ok(spec.trim_start_matches('v').to_string());
    }
    let index: Vec<IndexEntry> = fetcher.json(&format!("{DIST}/index.json")).await?;
    let lower = spec.to_ascii_lowercase();
    if lower.starts_with("lts") || lower == "node" || lower == "stable" || lower == "latest" || lower == "current" {
        let want_lts = lower.starts_with("lts");
        let codename = lower.strip_prefix("lts/").filter(|c| *c != "*").map(|s| s.to_string());
        let best = index
            .iter()
            .filter(|e| {
                if !want_lts {
                    return true;
                }
                match (&e.lts, &codename) {
                    (serde_json::Value::String(name), Some(c)) => name.to_ascii_lowercase() == *c,
                    (serde_json::Value::String(_), None) => true,
                    _ => false,
                }
            })
            .filter_map(|e| Version::parse(e.version.trim_start_matches('v')).ok())
            .max();
        return best.map(|v| v.to_string()).ok_or_else(|| anyhow!("no Node version matches {spec}"));
    }
    let versions: Vec<Version> =
        index.iter().filter_map(|e| Version::parse(e.version.trim_start_matches('v')).ok()).collect();
    best_match(spec, versions.iter()).map(|v| v.to_string()).ok_or_else(|| anyhow!("no Node version matches {spec:?}"))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Parts {
    pub npm: bool,
    pub corepack: bool,
    pub headers: bool,
}

pub async fn install(fetcher: &Fetcher, version: &str, dest: &Path, parts: Parts) -> Result<Installed> {
    let base = format!("node-v{version}-linux-{}", arch());
    let file = format!("{base}.tar.gz");
    let url = format!("{DIST}/v{version}/{file}");
    let sums_url = format!("{DIST}/v{version}/SHASUMS256.txt");
    let dest_owned = dest.to_path_buf();
    let headers = HeaderMap::new();
    let extract = fetcher.blob_streaming(&file, &url, &headers, None, move |r| {
        let gz = flate2::read::GzDecoder::new(r);
        let mut gz = gz;
        extract_tar_filtered(&mut gz, &dest_owned, 1, &|rel: &str| {
            if rel == "bin" || rel == "bin/node" || rel == "lib" || rel == "lib/node_modules" {
                return true;
            }
            if rel.starts_with("bin/") {
                return match rel {
                    "bin/npm" | "bin/npx" => parts.npm,
                    "bin/corepack" => parts.corepack,
                    _ => false,
                };
            }
            if rel.starts_with("lib/node_modules/npm") {
                return parts.npm;
            }
            if rel.starts_with("lib/node_modules/corepack") {
                return parts.corepack;
            }
            if rel.starts_with("include") {
                return parts.headers;
            }
            false
        })
    });
    let sums = fetcher.bytes(&sums_url);
    let (extracted, sums) = tokio::join!(extract, sums);
    let (_, blob) = extracted.with_context(|| format!("installing Node {version}"))?;
    let sums = String::from_utf8_lossy(&sums?).into_owned();
    let expected = parse_shasums(&sums, &file)?;
    if let Err(e) = verify(&blob, &expected, &file) {
        let _ = std::fs::remove_dir_all(dest);
        return Err(e);
    }
    Ok(Installed {
        name: "node".into(),
        version: version.to_string(),
        root: dest.to_path_buf(),
        bin_dir: dest.join("bin"),
        archive: Some(blob),
    })
}

pub fn image_tag(version: &str) -> String {
    format!("node:{version}-bookworm-slim")
}
