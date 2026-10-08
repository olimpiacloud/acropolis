use crate::ignore::Ignore;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

const SKIP: &[&str] = &[".lock", "src", "nm-prev", ".src-old-*", ".nm-old-*", "*.staging-*", "*.stale"];

pub fn sanitize_key(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn app_dir(home: &Path, key: &str) -> PathBuf {
    home.join("cache").join("apps").join(sanitize_key(key))
}

fn lock(dir: &Path) -> Result<File> {
    std::fs::create_dir_all(dir)?;
    let f = File::options().create(true).truncate(false).write(true).open(dir.join(".lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("the cache of {} is in use by a running build", dir.display());
    }
    Ok(f)
}

pub fn export(home: &Path, key: &str, out: &Path) -> Result<u64> {
    let dir = app_dir(home, key);
    if !dir.is_dir() {
        bail!("no cache for {key}");
    }
    let _lock = lock(&dir)?;
    let entries = crate::source::walk(&dir, &Ignore::new(&SKIP.iter().map(|s| s.to_string()).collect::<Vec<_>>()))?;
    let tmp = out.with_extension(format!("tmp-{}", std::process::id()));
    let file = BufWriter::with_capacity(1 << 20, File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    let mut enc = zstd::stream::write::Encoder::new(file, 3)?;
    let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let _ = enc.multithread(threads);
    crate::source::stream_tree_into(&dir, &entries, "", false, &mut enc)?;
    enc.write_all(&[0u8; 1024])?;
    enc.finish()?.flush()?;
    std::fs::rename(&tmp, out)?;
    Ok(std::fs::metadata(out)?.len())
}

pub fn import(home: &Path, key: &str, input: &Path) -> Result<u64> {
    let dir = app_dir(home, key);
    let _lock = lock(&dir)?;
    let staging = dir.with_extension(format!("import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let reader = zstd::stream::read::Decoder::new(BufReader::with_capacity(1 << 20, File::open(input).with_context(|| format!("opening {}", input.display()))?))?;
    let n = acropolis_oci::unpack::unpack_for_overlay(reader, &staging)?;
    for e in std::fs::read_dir(&staging)? {
        let e = e?;
        let target = dir.join(e.file_name());
        if target.file_name().is_some_and(|n| n == ".lock") {
            continue;
        }
        if std::fs::symlink_metadata(&target).is_ok() {
            if target.is_dir() {
                std::fs::remove_dir_all(&target)?;
            } else {
                std::fs::remove_file(&target)?;
            }
        }
        std::fs::rename(e.path(), &target)?;
    }
    let _ = std::fs::remove_dir_all(&staging);
    crate::gc::touch(&dir.join(".lock"));
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_skips_volatile_entries() {
        let base = std::env::temp_dir().join(format!("acropolis-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (base.join("a"), base.join("b"));
        let dir = app_dir(&a, "tenant/app");
        std::fs::create_dir_all(dir.join("gocache/x")).unwrap();
        std::fs::write(dir.join("gocache/x/obj"), b"compiled").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.go"), b"source").unwrap();
        std::os::unix::fs::symlink("x/obj", dir.join("gocache/link")).unwrap();
        let file = base.join("cache.tar.zst");
        export(&a, "tenant/app", &file).unwrap();
        import(&b, "tenant/app", &file).unwrap();
        let got = app_dir(&b, "tenant/app");
        assert_eq!(std::fs::read(got.join("gocache/x/obj")).unwrap(), b"compiled");
        assert_eq!(std::fs::read_link(got.join("gocache/link")).unwrap(), PathBuf::from("x/obj"));
        assert!(!got.join("src").exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
