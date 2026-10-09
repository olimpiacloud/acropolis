use crate::ignore::Ignore;
use acropolis_oci::layer::{self, Layer, LayerOptions};
use acropolis_oci::tar::TarWriter;
use acropolis_store::Store;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub enum EntryKind {
    Dir,
    File { size: u64, exec: bool },
    Symlink(String),
    Hardlink { target: String, exec: bool },
}

#[derive(Clone, Debug)]
pub struct SourceEntry {
    pub rel: String,
    pub kind: EntryKind,
}

type Inodes = std::collections::HashMap<(u64, u64), String>;

pub fn walk(root: &Path, ignore: &Ignore) -> Result<Vec<SourceEntry>> {
    let mut out = Vec::new();
    let mut inodes = Inodes::new();
    walk_dir(root, "", ignore, &mut out, &mut inodes)?;
    Ok(out)
}

fn walk_dir(dir: &Path, prefix: &str, ignore: &Ignore, out: &mut Vec<SourceEntry>, inodes: &mut Inodes) -> Result<()> {
    let mut names: Vec<(String, std::fs::Metadata)> = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = e.metadata()?;
        names.push((name, meta));
    }
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, meta) in names {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let excluded = ignore.excluded(&rel);
        let path = dir.join(&name);
        let ft = meta.file_type();
        if ft.is_dir() {
            if excluded && !ignore.has_negations() {
                continue;
            }
            let before = out.len();
            if !excluded {
                out.push(SourceEntry {
                    rel: rel.clone(),
                    kind: EntryKind::Dir,
                });
            }
            walk_dir(&path, &rel, ignore, out, inodes)?;
            if excluded && out.len() > before {
                out.insert(
                    before,
                    SourceEntry {
                        rel: rel.clone(),
                        kind: EntryKind::Dir,
                    },
                );
            }
        } else if excluded {
            continue;
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&path)?.to_string_lossy().into_owned();
            out.push(SourceEntry {
                rel,
                kind: EntryKind::Symlink(target),
            });
        } else if ft.is_file() {
            if meta.nlink() > 1 {
                if let Some(first) = inodes.get(&(meta.dev(), meta.ino())) {
                    let exec = meta.permissions().mode() & 0o111 != 0;
                    out.push(SourceEntry {
                        rel,
                        kind: EntryKind::Hardlink {
                            target: first.clone(),
                            exec,
                        },
                    });
                    continue;
                }
                inodes.insert((meta.dev(), meta.ino()), rel.clone());
            }
            let exec = meta.permissions().mode() & 0o111 != 0;
            out.push(SourceEntry {
                rel,
                kind: EntryKind::File {
                    size: meta.size(),
                    exec,
                },
            });
        }
    }
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

pub fn stream_tree_into<W: std::io::Write>(
    root: &Path,
    entries: &[SourceEntry],
    prefix: &str,
    with_ancestors: bool,
    out: &mut W,
) -> Result<()> {
    stream_tree(root, entries, prefix, with_ancestors, &mut |b| {
        out.write_all(&b)?;
        Ok(())
    })
}

fn stream_tree(
    root: &Path,
    entries: &[SourceEntry],
    prefix: &str,
    with_ancestors: bool,
    sink: &mut dyn FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
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
    sink(take(head))?;
    let window = (rayon::current_num_threads() * 2).max(2);
    for win in groups.chunks(window) {
        let bodies: Vec<Result<Vec<u8>>> = win
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
                    let dest = if prefix.is_empty() {
                        e.rel.clone()
                    } else {
                        format!("{prefix}/{}", e.rel)
                    };
                    match &e.kind {
                        EntryKind::Dir => tw.dir(&dest, 0o755)?,
                        EntryKind::Symlink(t) => tw.symlink(&dest, t)?,
                        EntryKind::Hardlink { target, exec } => {
                            let to = if prefix.is_empty() {
                                target.clone()
                            } else {
                                format!("{prefix}/{target}")
                            };
                            tw.hardlink_mode(&dest, &to, if *exec { 0o755 } else { 0o644 })?
                        }
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
            sink(b?)?;
        }
    }
    Ok(())
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
    let mut b = layer::LayerBuilder::new(store, comment, opts)?;
    stream_tree_into(root, &entries, prefix, true, &mut b)?;
    b.finish()
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
                EntryKind::Hardlink { .. } => Ok(0),
            }
        })
        .collect();
    for r in results {
        bytes += r?;
    }
    for e in &entries {
        if let EntryKind::Hardlink { target: first, .. } = &e.kind {
            let to = dst.join(&e.rel);
            let _ = std::fs::remove_file(&to);
            if std::fs::hard_link(dst.join(first), &to).is_err() {
                std::fs::copy(dst.join(first), &to)?;
            }
        }
    }
    acropolis_events::add_written(bytes);
    Ok(bytes)
}

pub fn stream_upper<W: std::io::Write>(
    upper: &Path,
    prefix: &str,
    include: &[String],
    exclude: &[String],
    out: &mut W,
) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let mut tw = TarWriter::new(out);
    for d in ancestors(prefix) {
        tw.dir(&d, 0o755)?;
    }
    let skip_always = [
        "proc",
        "dev",
        "sys",
        "etc/resolv.conf",
        "etc/hostname",
        "etc/hosts",
        "app",
        "tmp",
    ];
    fn opaque(p: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap_or_default();
        let mut buf = [0u8; 4];
        let n = unsafe {
            libc::lgetxattr(
                c.as_ptr(),
                c"trusted.overlay.opaque".as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        n > 0 && buf[0] == b'y'
    }
    #[allow(clippy::too_many_arguments)]
    fn walk_upper<W: std::io::Write>(
        dir: &Path,
        rel: &str,
        tw: &mut TarWriter<W>,
        prefix: &str,
        include: &[String],
        exclude: &[String],
        skip: &[&str],
        inodes: &mut Inodes,
    ) -> Result<()> {
        let mut names: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        names.sort();
        for n in names {
            let name = n.to_string_lossy().into_owned();
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if skip.contains(&r.as_str()) || exclude.iter().any(|e| r == *e || r.starts_with(&format!("{e}/"))) {
                continue;
            }
            let inside = include.is_empty()
                || include
                    .iter()
                    .any(|i| r == *i || r.starts_with(&format!("{i}/")) || i.starts_with(&format!("{r}/")));
            if !inside {
                continue;
            }
            let full = dir.join(&n);
            let meta = std::fs::symlink_metadata(&full)?;
            let ft = meta.file_type();
            let dest = if prefix.is_empty() {
                r.clone()
            } else {
                format!("{prefix}/{r}")
            };
            tw.set_owner(meta.uid(), meta.gid());
            if ft.is_char_device() && meta.rdev() == 0 {
                let parent = Path::new(&dest)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let wh = if parent.is_empty() {
                    format!(".wh.{name}")
                } else {
                    format!("{parent}/.wh.{name}")
                };
                tw.set_owner(0, 0);
                tw.file_bytes(&wh, 0o644, b"")?;
            } else if ft.is_dir() {
                tw.dir(&dest, meta.permissions().mode() & 0o7777)?;
                if opaque(&full) {
                    tw.set_owner(0, 0);
                    tw.file_bytes(&format!("{dest}/.wh..wh..opq"), 0o644, b"")?;
                }
                walk_upper(&full, &r, tw, prefix, include, exclude, skip, inodes)?;
            } else if ft.is_symlink() {
                let t = std::fs::read_link(&full)?.to_string_lossy().into_owned();
                tw.symlink(&dest, &t)?;
            } else if ft.is_file() {
                if meta.nlink() > 1 {
                    if let Some(first) = inodes.get(&(meta.dev(), meta.ino())) {
                        tw.hardlink_mode(&dest, first, meta.permissions().mode() & 0o7777)?;
                        continue;
                    }
                    inodes.insert((meta.dev(), meta.ino()), dest.clone());
                }
                let mut f = std::fs::File::open(&full)?;
                tw.file_reader(&dest, meta.permissions().mode() & 0o7777, meta.size(), &mut f)?;
            }
        }
        Ok(())
    }
    let mut inodes = Inodes::new();
    walk_upper(upper, "", &mut tw, prefix, include, exclude, &skip_always, &mut inodes)?;
    tw.set_owner(0, 0);
    Ok(())
}

#[cfg(test)]
mod hardlink_tests {
    use super::*;
    use acropolis_oci::tar::{Kind, TarReader};

    #[test]
    fn hardlinks_are_stored_once() {
        let dir = std::env::temp_dir().join(format!("acropolis-hardlinks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dri")).unwrap();
        std::fs::write(dir.join("dri/a.so"), vec![7u8; 100_000]).unwrap();
        std::fs::set_permissions(dir.join("dri/a.so"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::hard_link(dir.join("dri/a.so"), dir.join("dri/b.so")).unwrap();
        let entries = walk(&dir, &Ignore::new(&[])).unwrap();
        let mut out = Vec::new();
        stream_tree_into(&dir, &entries, "usr/lib", true, &mut out).unwrap();
        out.extend_from_slice(&[0u8; 1024]);
        let mut tr = TarReader::new(std::io::Cursor::new(out.clone()));
        let mut kinds = Vec::new();
        while let Some(e) = tr.next_entry().unwrap() {
            kinds.push((e.path.clone(), e.kind, e.link.clone()));
        }
        assert!(
            kinds
                .iter()
                .any(|(p, k, _)| p == "usr/lib/dri/a.so" && *k == Kind::File)
        );
        assert!(
            kinds
                .iter()
                .any(|(p, k, l)| p == "usr/lib/dri/b.so" && *k == Kind::Hardlink && l == "usr/lib/dri/a.so")
        );
        let mut tr = TarReader::new(std::io::Cursor::new(out.clone()));
        while let Some(e) = tr.next_entry().unwrap() {
            if e.kind == Kind::Hardlink {
                assert_eq!(e.mode & 0o777, 0o755);
            }
        }
        assert!(out.len() < 150_000);
        let copy = dir.with_extension("copy");
        copy_tree(&dir, &copy, &Ignore::new(&[])).unwrap();
        assert_eq!(std::fs::metadata(copy.join("dri/b.so")).unwrap().nlink(), 2);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&copy);
    }
}
