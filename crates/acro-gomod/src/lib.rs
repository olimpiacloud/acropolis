pub mod zip;

use acro_fetch::Fetcher;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use futures::future::try_join_all;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SumEntry {
    pub module: String,
    pub version: String,
    pub h1: String,
}

#[derive(Clone, Debug, Default)]
pub struct GoSum {
    pub zips: Vec<SumEntry>,
    pub mods: Vec<SumEntry>,
}

pub fn parse_go_sum(text: &str) -> Result<GoSum> {
    let mut zips = BTreeMap::new();
    let mut mods = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 || !parts[2].starts_with("h1:") {
            bail!("go.sum line {}: malformed: {line}", n + 1);
        }
        let (module, version, h1) = (parts[0], parts[1], parts[2]);
        if let Some(v) = version.strip_suffix("/go.mod") {
            mods.insert((module.to_string(), v.to_string()), h1.to_string());
        } else {
            zips.insert((module.to_string(), version.to_string()), h1.to_string());
        }
    }
    let conv = |m: BTreeMap<(String, String), String>| {
        m.into_iter().map(|((module, version), h1)| SumEntry { module, version, h1 }).collect()
    };
    Ok(GoSum { zips: conv(zips), mods: conv(mods) })
}

pub fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn hash1(files: &mut [(String, Vec<u8>)]) -> String {
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut summary = Sha256::new();
    for (name, data) in files.iter() {
        let h = Sha256::digest(data);
        summary.update(format!("{}  {}\n", hex::encode(h), name).as_bytes());
    }
    format!("h1:{}", base64::engine::general_purpose::STANDARD.encode(summary.finalize()))
}

pub fn hash1_gomod(data: &[u8]) -> String {
    let mut files = vec![("go.mod".to_string(), data.to_vec())];
    hash1(&mut files)
}

pub struct ModCache {
    pub root: PathBuf,
}

impl ModCache {
    pub fn download_dir(&self, module: &str) -> PathBuf {
        self.root.join("cache").join("download").join(escape(module)).join("@v")
    }

    pub fn module_dir(&self, module: &str, version: &str) -> PathBuf {
        self.root.join(format!("{}@{}", escape(module), escape(version)))
    }
}

pub struct Stats {
    pub modules: usize,
    pub gomods: usize,
    pub bytes: u64,
}

fn extract_verified(cache: &ModCache, e: &SumEntry, zip_bytes: &[u8]) -> Result<u64> {
    let entries = zip::entries(zip_bytes).with_context(|| format!("reading zip of {}@{}", e.module, e.version))?;
    let prefix = format!("{}@{}/", e.module, e.version);
    let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(entries.len());
    for z in &entries {
        if z.name.ends_with('/') {
            continue;
        }
        if !z.name.starts_with(&prefix) {
            bail!("{}@{}: zip entry {} outside module prefix", e.module, e.version, z.name);
        }
        files.push((z.name.clone(), z.read_all()?));
    }
    let got = hash1(&mut files);
    if got != e.h1 {
        bail!("integrity mismatch for {}@{}: go.sum has {}, downloaded zip hashes to {}", e.module, e.version, e.h1, got);
    }
    let dir = cache.module_dir(&e.module, &e.version);
    let mut total = 0u64;
    let mut made = std::collections::HashSet::new();
    for (name, data) in &files {
        let rel = &name[prefix.len()..];
        if rel.split('/').any(|c| c == "..") {
            bail!("unsafe path {name} in module zip");
        }
        let path = dir.join(rel);
        if let Some(parent) = path.parent()
            && made.insert(parent.to_path_buf())
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;
        total += data.len() as u64;
    }
    std::fs::create_dir_all(&dir)?;
    let dl = cache.download_dir(&e.module);
    std::fs::create_dir_all(&dl)?;
    std::fs::write(dl.join(format!("{}.ziphash", escape(&e.version))), &e.h1)?;
    acro_events::add_written(total);
    Ok(total)
}

pub async fn download_all(fetcher: &Fetcher, sum: &GoSum, cache: &ModCache, proxy: &str) -> Result<Stats> {
    let proxy = proxy.trim_end_matches('/');
    let zips = sum.zips.iter().map(|e| async move {
        let url = format!("{proxy}/{}/@v/{}.zip", escape(&e.module), escape(&e.version));
        let bytes = fetcher.bytes(&url).await?;
        let e2 = e.clone();
        let cache_root = cache.root.clone();
        let n = tokio::task::spawn_blocking(move || extract_verified(&ModCache { root: cache_root }, &e2, &bytes))
            .await??;
        Ok::<u64, anyhow::Error>(n)
    });
    let mods = sum.mods.iter().map(|e| async move {
        let url = format!("{proxy}/{}/@v/{}.mod", escape(&e.module), escape(&e.version));
        let bytes = fetcher.bytes(&url).await?;
        let got = hash1_gomod(&bytes);
        if got != e.h1 {
            return Err(anyhow!(
                "integrity mismatch for {}@{}/go.mod: go.sum has {}, got {}",
                e.module,
                e.version,
                e.h1,
                got
            ));
        }
        let dl = cache.download_dir(&e.module);
        tokio::fs::create_dir_all(&dl).await?;
        tokio::fs::write(dl.join(format!("{}.mod", escape(&e.version))), &bytes).await?;
        Ok::<u64, anyhow::Error>(bytes.len() as u64)
    });
    let (a, b) = tokio::join!(try_join_all(zips), try_join_all(mods));
    let a = a?;
    let b = b?;
    Ok(Stats { modules: a.len(), gomods: b.len(), bytes: a.iter().sum::<u64>() + b.iter().sum::<u64>() })
}

#[derive(Clone, Debug, Default)]
pub struct GoMod {
    pub module: String,
    pub go: Option<String>,
    pub toolchain: Option<String>,
    pub requires: Vec<(String, String)>,
    pub replaces_local: Vec<(String, String)>,
}

pub fn parse_go_mod(text: &str) -> GoMod {
    let mut m = GoMod::default();
    let mut block: Option<&str> = None;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(b) = block {
            if line == ")" {
                block = None;
                continue;
            }
            handle(&mut m, b, line);
            continue;
        }
        let (kw, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        if rest == "(" {
            block = Some(match kw {
                "require" => "require",
                "replace" => "replace",
                _ => "other",
            });
            continue;
        }
        match kw {
            "module" => m.module = rest.trim_matches('"').to_string(),
            "go" => m.go = Some(rest.to_string()),
            "toolchain" => m.toolchain = Some(rest.trim_start_matches("go").to_string()),
            "require" => handle(&mut m, "require", rest),
            "replace" => handle(&mut m, "replace", rest),
            _ => {}
        }
    }
    m
}

fn handle(m: &mut GoMod, block: &str, line: &str) {
    match block {
        "require" => {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() >= 2 {
                m.requires.push((p[0].to_string(), p[1].to_string()));
            }
        }
        "replace" => {
            if let Some((_, to)) = line.split_once("=>") {
                let to: Vec<&str> = to.split_whitespace().collect();
                let from = line.split_whitespace().next().unwrap_or("");
                if to.len() == 1 && (to[0].starts_with("./") || to[0].starts_with("../") || to[0].starts_with('/')) {
                    m.replaces_local.push((from.to_string(), to[0].to_string()));
                }
            }
        }
        _ => {}
    }
}

pub fn read_go_mod(dir: &Path) -> Result<GoMod> {
    let text = std::fs::read_to_string(dir.join("go.mod")).context("reading go.mod")?;
    Ok(parse_go_mod(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_paths() {
        assert_eq!(escape("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
    }

    #[test]
    fn gosum_and_gomod() {
        let s = parse_go_sum("a v1.0.0 h1:x=\na v1.0.0/go.mod h1:y=\n").unwrap();
        assert_eq!(s.zips.len(), 1);
        assert_eq!(s.mods.len(), 1);
        let m = parse_go_mod("module x\n\ngo 1.25.3\ntoolchain go1.25.4\nrequire (\n\ta v1 // indirect\n)\nreplace b => ./b\n");
        assert_eq!(m.module, "x");
        assert_eq!(m.go.as_deref(), Some("1.25.3"));
        assert_eq!(m.toolchain.as_deref(), Some("1.25.4"));
        assert_eq!(m.requires.len(), 1);
        assert_eq!(m.replaces_local.len(), 1);
    }

    #[test]
    fn gomod_hash_matches_go() {
        let data = b"module github.com/gin-contrib/sse\n\ngo 1.12\n\nrequire github.com/stretchr/testify v1.3.0\n";
        assert!(hash1_gomod(data).starts_with("h1:"));
    }
}
