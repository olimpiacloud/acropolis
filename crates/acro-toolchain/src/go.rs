use crate::semver_range::{best_match, parse_loose_version};
use crate::{Installed, extract_tar_filtered, verify};
use acro_fetch::Fetcher;
use acro_store::{Algo, Integrity};
use anyhow::{Context, Result, anyhow};
use reqwest::header::HeaderMap;
use semver::Version;
use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_GO: &str = "1.25";

#[derive(Deserialize)]
struct Release {
    version: String,
    #[serde(default)]
    stable: bool,
}

pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "amd64",
    }
}

fn go_version_to_semver(v: &str) -> Option<Version> {
    let v = v.trim_start_matches("go");
    if v.contains("rc") || v.contains("beta") {
        return None;
    }
    parse_loose_version(v)
}

pub fn is_full(spec: &str) -> bool {
    let s = spec.trim().trim_start_matches("go");
    s.split('.').count() == 3 && s.split('.').all(|p| p.parse::<u64>().is_ok())
}

pub async fn resolve(fetcher: &Fetcher, spec: &str) -> Result<String> {
    let spec = spec.trim().trim_start_matches("go");
    let spec = if spec.is_empty() { DEFAULT_GO } else { spec };
    if is_full(spec) {
        return Ok(spec.to_string());
    }
    let pick = |rels: &[Release]| -> Option<Version> {
        let vs: Vec<Version> =
            rels.iter().filter(|r| r.stable).filter_map(|r| go_version_to_semver(&r.version)).collect();
        let req = if spec.split('.').count() == 2 { format!("~{spec}") } else { spec.to_string() };
        best_match(&req, vs.iter())
    };
    let recent: Vec<Release> = fetcher.json("https://go.dev/dl/?mode=json").await?;
    if let Some(v) = pick(&recent) {
        return Ok(format_go(&v));
    }
    let all: Vec<Release> = fetcher.json("https://go.dev/dl/?mode=json&include=all").await?;
    pick(&all).map(|v| format_go(&v)).ok_or_else(|| anyhow!("no Go release matches {spec}"))
}

fn format_go(v: &Version) -> String {
    if v.minor >= 21 || v.major > 1 || v.patch > 0 {
        v.to_string()
    } else {
        format!("{}.{}", v.major, v.minor)
    }
}

pub fn keep_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    if matches!(first, "test" | "api" | "doc" | "misc") {
        return false;
    }
    if rel.starts_with("src/cmd/") && rel != "src/cmd/go.mod" {
        return false;
    }
    if rel.starts_with("lib/wasm") || rel.starts_with("pkg/tool/") && rel.contains("/cover") {
        return false;
    }
    if rel.ends_with("_test.go") || rel.contains("/testdata/") || rel.ends_with("/testdata") {
        return false;
    }
    true
}

pub async fn install(fetcher: &Fetcher, version: &str, dest: &Path) -> Result<Installed> {
    let file = format!("go{version}.linux-{}.tar.gz", arch());
    let url = format!("https://dl.google.com/go/{file}");
    let sum_url = format!("{url}.sha256");
    let dest_owned = dest.to_path_buf();
    let headers = HeaderMap::new();
    let extract = fetcher.blob_streaming(&file, &url, &headers, None, move |r| {
        let mut gz = flate2::read::GzDecoder::new(r);
        extract_tar_filtered(&mut gz, &dest_owned, 1, &keep_path)
    });
    let sum = fetcher.bytes(&sum_url);
    let (extracted, sum) = tokio::join!(extract, sum);
    let (_, blob) = extracted.with_context(|| format!("installing Go {version}"))?;
    let sum = sum?;
    let expected = Integrity::parse_hex(Algo::Sha256, String::from_utf8_lossy(&sum).trim())?;
    if let Err(e) = verify(&blob, &expected, &file) {
        let _ = std::fs::remove_dir_all(dest);
        return Err(e);
    }
    Ok(Installed {
        name: "go".into(),
        version: version.to_string(),
        root: dest.to_path_buf(),
        bin_dir: dest.join("bin"),
        archive: Some(blob),
    })
}
