use anyhow::Result;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn touch(path: &Path) {
    if let Ok(f) = File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

pub struct HomeLock {
    _file: File,
}

fn lock_file(home: &Path) -> Option<File> {
    std::fs::create_dir_all(home).ok()?;
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join(".gc.lock"))
        .ok()
}

pub fn shared(home: &Path) -> Option<HomeLock> {
    let f = lock_file(home)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH) };
    (rc == 0).then_some(HomeLock { _file: f })
}

fn exclusive(home: &Path) -> Option<HomeLock> {
    let f = lock_file(home)?;
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    (rc == 0).then_some(HomeLock { _file: f })
}

#[derive(Debug)]
struct Unit {
    path: PathBuf,
    size: u64,
    last_use: SystemTime,
    lock: Option<PathBuf>,
    marker: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct GcReport {
    pub before: u64,
    pub after: u64,
    pub removed: usize,
    pub exclusive: bool,
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                total += m.len();
            }
        }
    }
    total
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn units(home: &Path, rootfs: &Path) -> Vec<Unit> {
    let mut out = Vec::new();
    for algo in entries(&home.join("store").join("blobs")) {
        for shard in entries(&algo) {
            for blob in entries(&shard) {
                if blob.extension().is_some_and(|e| e == "sha256") {
                    continue;
                }
                let size = std::fs::metadata(&blob).map(|m| m.len()).unwrap_or(0);
                out.push(Unit {
                    last_use: mtime(&blob),
                    path: blob,
                    size,
                    lock: None,
                    marker: None,
                });
            }
        }
    }
    for t in entries(&home.join("toolchains")) {
        let marker = t.join(".acropolis-complete");
        out.push(Unit {
            size: dir_size(&t),
            last_use: mtime(&marker),
            path: t,
            lock: None,
            marker: Some(marker),
        });
    }
    for r in entries(rootfs) {
        let name = r
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.ends_with(".complete") || name.starts_with('.') || !r.is_dir() {
            continue;
        }
        let marker = rootfs.join(format!("{name}.complete"));
        out.push(Unit {
            size: dir_size(&r),
            last_use: mtime(&marker),
            path: r,
            lock: None,
            marker: Some(marker),
        });
    }
    for a in entries(&home.join("cache").join("apps")) {
        let lock = a.join(".lock");
        out.push(Unit {
            size: dir_size(&a),
            last_use: mtime(&lock),
            path: a,
            lock: Some(lock),
            marker: None,
        });
    }
    out
}

fn locked(lock: &Path) -> bool {
    let Ok(f) = File::options().write(true).open(lock) else {
        return false;
    };
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    rc != 0
}

fn remove(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sha256"));
    }
}

fn sweep_leftovers(home: &Path, older_than: Duration) {
    let now = SystemTime::now();
    let old = |p: &Path| now.duration_since(mtime(p)).map(|d| d > older_than).unwrap_or(false);
    for p in entries(&home.join("work"))
        .into_iter()
        .chain(entries(&home.join("store").join("tmp")))
    {
        if old(&p) {
            remove(&p);
        }
    }
    for dir in [home.join("toolchains"), home.join("cache").join("apps")] {
        for p in entries(&dir) {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if (name.starts_with('.') && (name.contains(".staging-") || name.ends_with(".stale"))) && old(&p) {
                remove(&p);
            }
        }
    }
    for app in entries(&home.join("cache").join("apps")) {
        for p in entries(&app) {
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".src-old-"))
                && old(&p)
            {
                remove(&p);
            }
        }
    }
}

pub fn collect(home: &Path, rootfs: &Path, max_size: u64) -> Result<GcReport> {
    let lock = exclusive(home);
    let exclusive = lock.is_some();
    let grace = if exclusive {
        Duration::from_secs(0)
    } else {
        Duration::from_secs(3600)
    };
    sweep_leftovers(
        home,
        if exclusive {
            Duration::from_secs(600)
        } else {
            Duration::from_secs(6 * 3600)
        },
    );
    let mut all = units(home, rootfs);
    let before: u64 = all.iter().map(|u| u.size).sum();
    let mut total = before;
    all.sort_by_key(|u| u.last_use);
    let now = SystemTime::now();
    let mut removed = 0;
    for u in all {
        if total <= max_size {
            break;
        }
        if now.duration_since(u.last_use).map(|d| d < grace).unwrap_or(true) {
            continue;
        }
        if u.lock.as_deref().is_some_and(locked) {
            continue;
        }
        if let Some(m) = &u.marker {
            let _ = std::fs::remove_file(m);
        }
        remove(&u.path);
        total = total.saturating_sub(u.size);
        removed += 1;
    }
    drop(lock);
    Ok(GcReport {
        before,
        after: total,
        removed,
        exclusive,
    })
}

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1 << 30),
        'T' | 't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.trim().parse::<f64>().ok().map(|n| (n * mul as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10G"), Some(10 << 30));
        assert_eq!(parse_size("512M"), Some(512 << 20));
        assert_eq!(parse_size("1.5g"), Some((1.5 * (1u64 << 30) as f64) as u64));
        assert_eq!(parse_size("100"), Some(100));
    }

    #[test]
    fn evicts_least_recently_used_first() {
        let home = std::env::temp_dir().join(format!("acropolis-gc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let shard = home.join("store/blobs/sha256/ab");
        std::fs::create_dir_all(&shard).unwrap();
        let old = shard.join("ab01");
        let new = shard.join("ab02");
        std::fs::write(&old, vec![0u8; 4096]).unwrap();
        std::fs::write(&new, vec![0u8; 4096]).unwrap();
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        let r = collect(&home, &home.join("rootfs"), 5000).unwrap();
        assert_eq!(r.removed, 1);
        assert!(!old.exists());
        assert!(new.exists());
        let _ = std::fs::remove_dir_all(&home);
    }
}
