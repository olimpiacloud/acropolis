use crate::ignore::Ignore;
use acro_oci::layer::{self, Layer, LayerOptions};
use acro_oci::tar::TarWriter;
use acro_store::Store;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub enum EntryKind {
    Dir,
    File { size: u64, exec: bool },
    Symlink(String),
}

#[derive(Clone, Debug)]
pub struct SourceEntry {
    pub rel: String,
    pub kind: EntryKind,
}

pub fn walk(root: &Path, ignore: &Ignore) -> Result<Vec<SourceEntry>> {
    let mut out = Vec::new();
    walk_dir(root, root, "", ignore, &mut out)?;
    Ok(out)
}

fn walk_dir(root: &Path, dir: &Path, prefix: &str, ignore: &Ignore, out: &mut Vec<SourceEntry>) -> Result<()> {
    let mut names: Vec<(String, std::fs::Metadata)> = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(e.path())?;
        names.push((name, meta));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, meta) in names {
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let excluded = ignore.excluded(&rel);
        let path = dir.join(&name);
        let ft = meta.file_type();
        if ft.is_dir() {
            if excluded && !ignore.has_negations() {
                continue;
            }
            let before = out.len();
            if !excluded {
                out.push(SourceEntry { rel: rel.clone(), kind: EntryKind::Dir });
            }
            walk_dir(root, &path, &rel, ignore, out)?;
            if excluded && out.len() > before {
                out.insert(before, SourceEntry { rel: rel.clone(), kind: EntryKind::Dir });
            }
        } else if excluded {
            continue;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&path)?.to_string_lossy().into_owned();
            out.push(SourceEntry { rel, kind: EntryKind::Symlink(target) });
        } else if ft.is_file() {
            let exec = meta.permissions().mode() & 0o111 != 0;
            out.push(SourceEntry { rel, kind: EntryKind::File { size: meta.size(), exec } });
        }
    }
    let _ = root;
    Ok(())
}

pub fn ancestors(prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for part in prefix.split('/').filter(|p| !p.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        out.push(acc.clone());
    }
    out
}

pub fn fragments_for(root: &Path, entries: &[SourceEntry], prefix: &str, with_ancestors: bool) -> Result<Vec<Vec<u8>>> {
    const TARGET: u64 = 2 << 20;
    let mut groups: Vec<&[SourceEntry]> = Vec::new();
    let mut start = 0;
    let mut acc = 0u64;
    for (i, e) in entries.iter().enumerate() {
        acc += match e.kind {
            EntryKind::File { size, .. } => size + 512,
            _ => 512,
        };
        if acc >= TARGET {
            groups.push(&entries[start..=i]);
            start = i + 1;
            acc = 0;
        }
    }
    if start < entries.len() {
        groups.push(&entries[start..]);
    }
    let mut head = TarWriter::new(Vec::new());
    if with_ancestors {
        for d in ancestors(prefix) {
            head.dir(&d, 0o755)?;
        }
    }
    let mut frags = vec![take(head)];
    let bodies: Vec<Result<Vec<u8>>> = groups
        .par_iter()
        .map(|g| {
            let cap: u64 = g
                .iter()
                .map(|e| match e.kind {
                    EntryKind::File { size, .. } => size + 1024,
                    _ => 512,
                })
                .sum();
            let mut tw = TarWriter::new(Vec::with_capacity(cap as usize));
            for e in g.iter() {
                let dest = if prefix.is_empty() { e.rel.clone() } else { format!("{prefix}/{}", e.rel) };
                match &e.kind {
                    EntryKind::Dir => tw.dir(&dest, 0o755)?,
                    EntryKind::Symlink(t) => tw.symlink(&dest, t)?,
                    EntryKind::File { size, exec } => {
                        let mut f = std::fs::File::open(root.join(&e.rel))
                            .with_context(|| format!("opening {}", root.join(&e.rel).display()))?;
                        tw.file_reader(&dest, if *exec { 0o755 } else { 0o644 }, *size, &mut f)?;
                    }
                }
            }
            Ok(take(tw))
        })
        .collect();
    for b in bodies {
        frags.push(b?);
    }
    Ok(frags)
}

fn take(mut tw: TarWriter<Vec<u8>>) -> Vec<u8> {
    std::mem::take(tw.get_mut())
}

pub fn dir_layer(
    store: &Store,
    root: &Path,
    ignore: &Ignore,
    prefix: &str,
    comment: &str,
    opts: LayerOptions,
) -> Result<Layer> {
    let entries = walk(root, ignore)?;
    let frags = fragments_for(root, &entries, prefix, true)?;
    layer::from_fragments(store, comment, frags, opts)
}

pub fn copy_tree(src: &Path, dst: &Path, ignore: &Ignore) -> Result<u64> {
    let entries = walk(src, ignore)?;
    std::fs::create_dir_all(dst)?;
    let mut bytes = 0;
    for e in &entries {
        if let EntryKind::Dir = e.kind {
            std::fs::create_dir_all(dst.join(&e.rel))?;
        }
    }
    let results: Vec<Result<u64>> = entries
        .par_iter()
        .map(|e| {
            let to: PathBuf = dst.join(&e.rel);
            match &e.kind {
                EntryKind::Dir => Ok(0),
                EntryKind::Symlink(t) => {
                    let _ = std::fs::remove_file(&to);
                    std::os::unix::fs::symlink(t, &to)?;
                    Ok(0)
                }
                EntryKind::File { size, .. } => {
                    std::fs::copy(src.join(&e.rel), &to)?;
                    Ok(*size)
                }
            }
        })
        .collect();
    for r in results {
        bytes += r?;
    }
    acro_events::add_written(bytes);
    Ok(bytes)
}
