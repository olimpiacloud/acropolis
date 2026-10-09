use crate::Installed;
use acropolis_fetch::Fetcher;
use acropolis_store::{Algo, Integrity};
use anyhow::{Context, Result, anyhow, bail};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub struct UvRelease {
    pub version: String,
    pub url: String,
    pub sha256: Integrity,
}

fn wheel_tag() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "manylinux_2_17_aarch64",
        _ => "manylinux_2_17_x86_64",
    }
}

pub async fn resolve(fetcher: &Fetcher, spec: &str) -> Result<UvRelease> {
    let spec = spec.trim();
    let url = if spec.is_empty() || spec == "latest" {
        "https://pypi.org/pypi/uv/json".to_string()
    } else {
        format!("https://pypi.org/pypi/uv/{spec}/json")
    };
    let v: serde_json::Value = fetcher
        .json(&url)
        .await
        .context("fetching uv release metadata from PyPI")?;
    let version = v["info"]["version"]
        .as_str()
        .ok_or_else(|| anyhow!("no uv version"))?
        .to_string();
    let files = v["urls"].as_array().cloned().unwrap_or_default();
    let tag = wheel_tag();
    let f = files
        .iter()
        .find(|f| {
            f["filename"]
                .as_str()
                .map(|n| n.contains(tag) && n.ends_with(".whl"))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("uv {version} has no {tag} wheel"))?;
    Ok(UvRelease {
        version,
        url: f["url"].as_str().unwrap_or("").to_string(),
        sha256: Integrity::parse_hex(Algo::Sha256, f["digests"]["sha256"].as_str().unwrap_or(""))?,
    })
}

pub async fn install(fetcher: &Fetcher, release: &UvRelease, dest: &Path) -> Result<Installed> {
    let blob = fetcher
        .blob("uv wheel", &release.url, Some(release.sha256.clone()))
        .await?;
    // Inflating the ~40 MB binary is CPU and disk work: keep it off the async workers.
    let (path, bin) = (blob.path.clone(), dest.join("bin"));
    let bin = tokio::task::spawn_blocking(move || -> Result<_> {
        let data = std::fs::read(&path)?;
        let entries = acropolis_gomod::zip::entries(&data)?;
        std::fs::create_dir_all(&bin)?;
        let mut found = 0;
        for e in &entries {
            let name = e.name.rsplit('/').next().unwrap_or("");
            if e.name.contains(".data/scripts/") && (name == "uv" || name == "uvx") {
                let bytes = e.read_all()?;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o755)
                    .open(bin.join(name))?;
                f.write_all(&bytes)?;
                found += 1;
            }
        }
        if found == 0 {
            bail!("uv wheel has no binaries");
        }
        Ok(bin)
    })
    .await??;
    Ok(Installed {
        name: "uv".into(),
        version: release.version.clone(),
        root: dest.to_path_buf(),
        bin_dir: bin,
        archive: Some(blob),
    })
}
