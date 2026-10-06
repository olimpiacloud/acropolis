use crate::Installed;
use crate::semver_range::best_match;
use acro_fetch::Fetcher;
use acro_oci::tar::{Kind, TarReader};
use acro_store::Integrity;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use semver::Version;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub fn package_name() -> &'static str {
    let avx2 = {
        #[cfg(target_arch = "x86_64")]
        {
            std::is_x86_feature_detected!("avx2")
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            true
        }
    };
    match (std::env::consts::ARCH, avx2) {
        ("aarch64", _) => "@oven/bun-linux-aarch64",
        (_, true) => "@oven/bun-linux-x64",
        (_, false) => "@oven/bun-linux-x64-baseline",
    }
}

pub struct BunRelease {
    pub version: String,
    pub tarball: String,
    pub integrity: Option<Integrity>,
}

pub async fn resolve(fetcher: &Fetcher, spec: &str) -> Result<BunRelease> {
    let pkg = package_name();
    let url = format!("https://registry.npmjs.org/{}", pkg.replace('/', "%2f"));
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.npm.install-v1+json"));
    let body = fetcher.bytes_with(&url, &headers).await?;
    let v: serde_json::Value = serde_json::from_slice(&body).context("parsing bun package metadata")?;
    let spec = spec.trim().trim_start_matches('v');
    let version = if spec.is_empty() || spec == "latest" {
        v["dist-tags"]["latest"].as_str().map(|s| s.to_string())
    } else {
        let versions: Vec<Version> = v["versions"]
            .as_object()
            .map(|m| m.keys().filter_map(|k| Version::parse(k).ok()).collect())
            .unwrap_or_default();
        best_match(spec, versions.iter()).map(|v| v.to_string())
    }
    .ok_or_else(|| anyhow!("no Bun version matches {spec:?}"))?;
    let dist = &v["versions"][&version]["dist"];
    let tarball = dist["tarball"].as_str().ok_or_else(|| anyhow!("bun {version} has no tarball"))?.to_string();
    let integrity = dist["integrity"].as_str().map(Integrity::parse_sri).transpose()?;
    Ok(BunRelease { version, tarball, integrity })
}

pub async fn install(fetcher: &Fetcher, release: &BunRelease, dest: &Path) -> Result<Installed> {
    let headers = HeaderMap::new();
    let dest_owned = dest.to_path_buf();
    let (found, blob) = fetcher
        .blob_streaming("bun", &release.tarball, &headers, release.integrity.clone(), move |r| {
            let gz = flate2::read::GzDecoder::new(r);
            let mut tr = TarReader::new(std::io::BufReader::new(gz));
            let bin = dest_owned.join("bin");
            std::fs::create_dir_all(&bin)?;
            let mut found = false;
            while let Some(e) = tr.next_entry()? {
                if e.kind == Kind::File && e.path.ends_with("bin/bun") {
                    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(bin.join("bun"))?;
                    let mut buf = vec![0u8; 256 * 1024];
                    let mut data = tr.data();
                    loop {
                        let n = data.read(&mut buf)?;
                        if n == 0 {
                            break;
                        }
                        f.write_all(&buf[..n])?;
                    }
                    found = true;
                }
            }
            Ok(found)
        })
        .await?;
    if !found {
        bail!("bun tarball has no bin/bun");
    }
    let _ = std::fs::remove_file(dest.join("bin/bunx"));
    std::os::unix::fs::symlink("bun", dest.join("bin/bunx"))?;
    Ok(Installed {
        name: "bun".into(),
        version: release.version.clone(),
        root: dest.to_path_buf(),
        bin_dir: dest.join("bin"),
        archive: Some(blob),
    })
}
