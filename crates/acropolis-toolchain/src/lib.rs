pub mod bun;
pub mod go;
pub mod node;
pub mod npmpkg;
pub mod uv;
pub use acropolis_semver as semver_range;

use acropolis_oci::tar::{Kind, TarReader};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub archive: Option<acropolis_store::StoredBlob>,
}

pub struct ExtractStats {
    pub files: u64,
    pub bytes: u64,
}

pub fn extract_tar_filtered(
    reader: &mut dyn Read,
    dest: &Path,
    strip: usize,
    keep: &dyn Fn(&str) -> bool,
) -> Result<ExtractStats> {
    std::fs::create_dir_all(dest)?;
    let mut tr = TarReader::new(std::io::BufReader::with_capacity(256 * 1024, reader));
    let mut made: HashSet<PathBuf> = HashSet::new();
    let mut stats = ExtractStats { files: 0, bytes: 0 };
    let mut buf = vec![0u8; 256 * 1024];
    let mut links: Vec<(PathBuf, String)> = Vec::new();
    while let Some(e) = tr.next_entry()? {
        let rel: Vec<&str> = e
            .path
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .skip(strip)
            .collect();
        if rel.is_empty() || rel.contains(&"..") {
            continue;
        }
        let rel = rel.join("/");
        if !keep(&rel) {
            continue;
        }
        let path = dest.join(&rel);
        match e.kind {
            Kind::Dir => {
                if made.insert(path.clone()) {
                    std::fs::create_dir_all(&path)?;
                }
            }
            Kind::File => {
                if let Some(parent) = path.parent()
                    && made.insert(parent.to_path_buf())
                {
                    std::fs::create_dir_all(parent)?;
                }
                let mode = if e.mode & 0o111 != 0 { 0o755 } else { 0o644 };
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(mode)
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                let mut data = tr.data();
                loop {
                    let n = data.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    f.write_all(&buf[..n])?;
                    stats.bytes += n as u64;
                }
                stats.files += 1;
            }
            Kind::Symlink => {
                if let Some(parent) = path.parent()
                    && made.insert(parent.to_path_buf())
                {
                    std::fs::create_dir_all(parent)?;
                }
                links.push((path, e.link.clone()));
            }
            Kind::Hardlink => {
                let target: Vec<&str> = e.link.split('/').filter(|c| !c.is_empty()).skip(strip).collect();
                let target = dest.join(target.join("/"));
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let _ = std::fs::remove_file(&path);
                if std::fs::hard_link(&target, &path).is_err() {
                    std::fs::copy(&target, &path)?;
                }
            }
        }
    }
    for (path, target) in links {
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(&target, &path)?;
    }
    acropolis_events::add_written(stats.bytes);
    Ok(stats)
}

pub fn parse_shasums(text: &str, file: &str) -> Result<acropolis_store::Integrity> {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(hash), Some(name)) = (it.next(), it.next())
            && name.trim_start_matches('*') == file
        {
            return Ok(acropolis_store::Integrity::parse_hex(
                acropolis_store::Algo::Sha256,
                hash,
            )?);
        }
    }
    bail!("{file} not listed in checksums")
}

pub fn verify(blob: &acropolis_store::StoredBlob, expected: &acropolis_store::Integrity, what: &str) -> Result<()> {
    if &blob.sha256 != expected {
        let _ = std::fs::remove_file(&blob.path);
        bail!(
            "integrity mismatch for {what}: expected {}, got {}",
            expected.to_oci(),
            blob.sha256.to_oci()
        );
    }
    Ok(())
}
