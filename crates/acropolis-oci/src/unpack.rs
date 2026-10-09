use crate::tar::{Kind, TarReader};
use anyhow::{Context, Result};
use std::ffi::CString;
use std::io::{BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for c in Path::new(rel.trim_start_matches('/')).components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn symlinked_ancestor(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return true };
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for c in comps.iter().take(comps.len().saturating_sub(1)) {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
}

pub fn open_layer(path: &Path, media_type: &str) -> Result<Box<dyn Read>> {
    let f = BufReader::with_capacity(256 * 1024, std::fs::File::open(path)?);
    Ok(if media_type.contains("zstd") {
        Box::new(zstd::stream::read::Decoder::new(f)?)
    } else if media_type.contains("gzip") || media_type.ends_with("tar.gzip") {
        Box::new(flate2::read::MultiGzDecoder::new(f))
    } else {
        Box::new(f)
    })
}

/// Unpacks an image layer for use as an overlay lowerdir: OCI whiteouts become overlay ones.
pub fn unpack_for_overlay<R: Read>(reader: R, dest: &Path) -> Result<u64> {
    unpack(reader, dest, true)
}

/// Unpacks a tar as plain files: `.wh.*` names are ordinary files (e.g. inside an app cache).
pub fn unpack_plain<R: Read>(reader: R, dest: &Path) -> Result<u64> {
    unpack(reader, dest, false)
}

fn unpack<R: Read>(reader: R, dest: &Path, whiteouts: bool) -> Result<u64> {
    std::fs::create_dir_all(dest)?;
    let mut tr = TarReader::new(BufReader::with_capacity(256 * 1024, reader));
    let mut total = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
    while let Some(e) = tr.next_entry()? {
        let Some(path) = safe_join(dest, &e.path) else { continue };
        if symlinked_ancestor(dest, &path) || (path == dest && e.kind != Kind::Dir) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if whiteouts && name == ".wh..wh..opq" {
            if let Some(parent) = path.parent() {
                let p = cstr(parent);
                unsafe {
                    libc::setxattr(
                        p.as_ptr(),
                        c"trusted.overlay.opaque".as_ptr(),
                        b"y".as_ptr() as *const libc::c_void,
                        1,
                        0,
                    );
                }
            }
            continue;
        }
        if let Some(target) = name.strip_prefix(".wh.")
            && whiteouts
        {
            if matches!(target, "" | "." | "..") {
                continue;
            }
            let wpath = path.with_file_name(target);
            let _ = std::fs::remove_dir_all(&wpath);
            let _ = std::fs::remove_file(&wpath);
            let p = cstr(&wpath);
            unsafe {
                libc::mknod(p.as_ptr(), libc::S_IFCHR, libc::makedev(0, 0));
            }
            continue;
        }
        match e.kind {
            Kind::Dir => {
                std::fs::create_dir_all(&path)?;
                dir_modes.push((path.clone(), e.mode));
                let p = cstr(&path);
                unsafe {
                    libc::lchown(p.as_ptr(), e.uid, e.gid);
                }
            }
            Kind::File => {
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(e.mode & 0o7777)
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                let mut data = tr.data();
                loop {
                    let n = data.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    f.write_all(&buf[..n])?;
                    total += n as u64;
                }
                {
                    use std::os::unix::io::AsRawFd;
                    let fd = f.as_raw_fd();
                    unsafe {
                        libc::fchown(fd, e.uid, e.gid);
                        libc::fchmod(fd, e.mode & 0o7777);
                    }
                }
                drop(f);
            }
            Kind::Symlink => {
                let _ = std::fs::remove_file(&path);
                std::os::unix::fs::symlink(&e.link, &path)?;
                let p = cstr(&path);
                unsafe {
                    libc::lchown(p.as_ptr(), e.uid, e.gid);
                }
            }
            Kind::Hardlink => {
                if let Some(target) = safe_join(dest, &e.link)
                    && !symlinked_ancestor(dest, &target)
                    && std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_file())
                {
                    let _ = std::fs::remove_file(&path);
                    if std::fs::hard_link(&target, &path).is_err() {
                        let _ = std::fs::copy(&target, &path);
                    }
                }
            }
        }
    }
    for (p, mode) in dir_modes.into_iter().rev() {
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) && !symlinked_ancestor(dest, &p) {
            let c = cstr(&p);
            unsafe {
                libc::chmod(c.as_ptr(), mode & 0o7777);
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::TarWriter;

    #[test]
    fn layer_cannot_write_through_its_own_symlinks() {
        let base = std::env::temp_dir().join(format!("acropolis-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let outside = base.join("outside");
        let dest = base.join("dest");
        std::fs::create_dir_all(&outside).unwrap();
        let mut tw = TarWriter::new(Vec::new());
        tw.symlink("a", outside.to_str().unwrap()).unwrap();
        tw.file_bytes("a/pwned", 0o644, b"x").unwrap();
        tw.file_bytes("ok/file", 0o644, b"y").unwrap();
        tw.symlink("ok/link", "/etc/hostname").unwrap();
        let data = tw.finish().unwrap();
        unpack_for_overlay(std::io::Cursor::new(data), &dest).unwrap();
        assert!(!outside.join("pwned").exists());
        assert_eq!(std::fs::read(dest.join("ok/file")).unwrap(), b"y");
        assert!(
            std::fs::symlink_metadata(dest.join("ok/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn whiteouts_and_root_entries_stay_inside_the_destination() {
        let base = std::env::temp_dir().join(format!("acropolis-unpack-wh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let dest = base.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(base.join("sibling"), b"keep").unwrap();
        std::fs::write(dest.join("kept"), b"keep").unwrap();
        let mut tw = TarWriter::new(Vec::new());
        tw.file_bytes(".wh...", 0o644, b"").unwrap();
        tw.file_bytes(".wh..", 0o644, b"").unwrap();
        tw.file_bytes(".wh.", 0o644, b"").unwrap();
        tw.file_bytes(".", 0o644, b"x").unwrap();
        tw.file_bytes("sub/file", 0o644, b"y").unwrap();
        let data = tw.finish().unwrap();
        unpack_for_overlay(std::io::Cursor::new(data), &dest).unwrap();
        assert_eq!(std::fs::read(base.join("sibling")).unwrap(), b"keep");
        assert_eq!(std::fs::read(dest.join("kept")).unwrap(), b"keep");
        assert_eq!(std::fs::read(dest.join("sub/file")).unwrap(), b"y");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn plain_unpack_keeps_whiteout_names_as_files() {
        let base = std::env::temp_dir().join(format!("acropolis-unpack-plain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut tw = TarWriter::new(Vec::new());
        tw.file_bytes("foo", 0o644, b"foo").unwrap();
        tw.file_bytes(".wh.foo", 0o644, b"wh").unwrap();
        tw.file_bytes("d/.wh..wh..opq", 0o644, b"opq").unwrap();
        tw.symlink("link", "/etc").unwrap();
        tw.file_bytes("link/pwned", 0o644, b"x").unwrap();
        let data = tw.finish().unwrap();
        unpack_plain(std::io::Cursor::new(data), &base).unwrap();
        assert_eq!(std::fs::read(base.join("foo")).unwrap(), b"foo");
        assert_eq!(std::fs::read(base.join(".wh.foo")).unwrap(), b"wh");
        assert_eq!(std::fs::read(base.join("d/.wh..wh..opq")).unwrap(), b"opq");
        assert!(!Path::new("/etc/pwned").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
