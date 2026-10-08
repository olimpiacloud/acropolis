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

pub fn unpack_for_overlay<R: Read>(reader: R, dest: &Path) -> Result<u64> {
    std::fs::create_dir_all(dest)?;
    let mut tr = TarReader::new(BufReader::with_capacity(256 * 1024, reader));
    let mut total = 0u64;
    let mut buf = vec![0u8; 256 * 1024];
    let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();
    while let Some(e) = tr.next_entry()? {
        let Some(path) = safe_join(dest, &e.path) else { continue };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if name == ".wh..wh..opq" {
            if let Some(parent) = path.parent() {
                let p = cstr(parent);
                unsafe {
                    libc::setxattr(p.as_ptr(), c"trusted.overlay.opaque".as_ptr(), b"y".as_ptr() as *const libc::c_void, 1, 0);
                }
            }
            continue;
        }
        if let Some(target) = name.strip_prefix(".wh.") {
            let wpath = path.with_file_name(target);
            let _ = std::fs::remove_dir_all(&wpath);
            let _ = std::fs::remove_file(&wpath);
            let p = cstr(&wpath);
            unsafe {
                libc::mknod(p.as_ptr(), libc::S_IFCHR | 0o000, libc::makedev(0, 0));
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
                drop(f);
                let p = cstr(&path);
                unsafe {
                    libc::lchown(p.as_ptr(), e.uid, e.gid);
                    libc::chmod(p.as_ptr(), e.mode & 0o7777);
                }
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
                if let Some(target) = safe_join(dest, &e.link) {
                    let _ = std::fs::remove_file(&path);
                    if std::fs::hard_link(&target, &path).is_err() {
                        let _ = std::fs::copy(&target, &path);
                    }
                }
            }
        }
    }
    for (p, mode) in dir_modes.into_iter().rev() {
        let c = cstr(&p);
        unsafe {
            libc::chmod(c.as_ptr(), mode & 0o7777);
        }
    }
    Ok(total)
}
