use crate::Installed;
use crate::semver_range::best_match;
use acropolis_fetch::Fetcher;
use acropolis_store::Integrity;
use anyhow::{Context, Result, anyhow};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use semver::Version;
use std::path::Path;

pub struct NpmRelease {
    pub name: String,
    pub version: String,
    pub tarball: String,
    pub integrity: Integrity,
    pub bin: serde_json::Value,
}

pub async fn resolve(fetcher: &Fetcher, name: &str, spec: &str) -> Result<NpmRelease> {
    let url = format!("https://registry.npmjs.org/{}", name.replace('/', "%2f"));
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.npm.install-v1+json"));
    let body = fetcher.bytes_with(&url, &headers).await?;
    let v: serde_json::Value = serde_json::from_slice(&body).with_context(|| format!("parsing metadata of {name}"))?;
    let spec = spec.trim();
    let version = if let Some(t) = v["dist-tags"]
        .get(if spec.is_empty() { "latest" } else { spec })
        .and_then(|t| t.as_str())
    {
        t.to_string()
    } else {
        let versions: Vec<Version> = v["versions"]
            .as_object()
            .map(|m| m.keys().filter_map(|k| Version::parse(k).ok()).collect())
            .unwrap_or_default();
        best_match(spec, versions.iter())
            .map(|v| v.to_string())
            .ok_or_else(|| anyhow!("no {name} version matches {spec:?}"))?
    };
    let meta = &v["versions"][&version];
    let dist = &meta["dist"];
    Ok(NpmRelease {
        name: name.to_string(),
        version: version.clone(),
        tarball: dist["tarball"]
            .as_str()
            .ok_or_else(|| anyhow!("{name}@{version} has no tarball"))?
            .to_string(),
        integrity: crate::npm_dist_integrity(dist).with_context(|| format!("{name}@{version}"))?,
        bin: meta["bin"].clone(),
    })
}

pub async fn install(fetcher: &Fetcher, release: &NpmRelease, dest: &Path) -> Result<Installed> {
    let headers = HeaderMap::new();
    let pkg_dir = dest.join("lib/node_modules").join(&release.name);
    let pkg2 = pkg_dir.clone();
    let (_, blob) = fetcher
        .blob_streaming(
            &release.name,
            &release.tarball,
            &headers,
            Some(release.integrity.clone()),
            move |r| {
                let mut gz = flate2::read::GzDecoder::new(r);
                crate::extract_tar_filtered(&mut gz, &pkg2, 1, &|_| true)
            },
        )
        .await?;
    let bin_dir = dest.join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let short = release.name.rsplit('/').next().unwrap_or(&release.name).to_string();
    let bins: Vec<(String, String)> = match &release.bin {
        serde_json::Value::String(s) => vec![(short, s.clone())],
        serde_json::Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => Vec::new(),
    };
    for (name, target) in bins {
        let target = target.trim_start_matches("./");
        if name.is_empty() || name.contains('/') || name == ".." || target.split('/').any(|c| c == "..") {
            continue;
        }
        let link = bin_dir.join(&name);
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(format!("../lib/node_modules/{}/{target}", release.name), &link)?;
        let full = pkg_dir.join(target);
        if let Ok(meta) = std::fs::symlink_metadata(&full)
            && meta.is_file()
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = meta.permissions();
            p.set_mode(0o755);
            let _ = std::fs::set_permissions(&full, p);
        }
    }
    Ok(Installed {
        name: release.name.clone(),
        version: release.version.clone(),
        root: dest.to_path_buf(),
        bin_dir,
        archive: Some(blob),
    })
}
