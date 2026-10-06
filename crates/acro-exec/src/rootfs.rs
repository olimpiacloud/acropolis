use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Bind {
    pub host: PathBuf,
    pub guest: String,
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub struct RootfsRun {
    pub step: String,
    pub lower: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub merged: PathBuf,
    pub binds: Vec<Bind>,
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub network: bool,
}

struct Prepared {
    merged: CString,
    overlay_opts: CString,
    binds: Vec<(CString, CString, bool)>,
    proc_target: CString,
    dev_target: CString,
    sys_target: CString,
    resolv_src: Option<CString>,
    resolv_target: CString,
    cwd: CString,
    network: bool,
}

fn cpath(p: &Path) -> Result<CString> {
    Ok(CString::new(p.as_os_str().as_bytes())?)
}

fn check(rc: libc::c_int) -> std::io::Result<()> {
    if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn enter(p: &Prepared) -> std::io::Result<()> {
    unsafe {
        let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
        if !p.network {
            flags |= libc::CLONE_NEWNET;
        }
        check(libc::unshare(flags))?;
        check(libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()))?;
        check(libc::mount(
            c"overlay".as_ptr(),
            p.merged.as_ptr(),
            c"overlay".as_ptr(),
            0,
            p.overlay_opts.as_ptr() as *const libc::c_void,
        ))?;
        check(libc::mount(c"/proc".as_ptr(), p.proc_target.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()))?;
        check(libc::mount(c"/dev".as_ptr(), p.dev_target.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()))?;
        let _ = libc::mount(c"/sys".as_ptr(), p.sys_target.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null());
        if let Some(src) = &p.resolv_src {
            let _ = libc::mount(src.as_ptr(), p.resolv_target.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null());
        }
        for (host, guest, ro) in &p.binds {
            check(libc::mount(host.as_ptr(), guest.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()))?;
            if *ro {
                let _ = libc::mount(
                    std::ptr::null(),
                    guest.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                );
            }
        }
        check(libc::chroot(p.merged.as_ptr()))?;
        check(libc::chdir(p.cwd.as_ptr()))?;
        if !p.network {
            let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if s >= 0 {
                let mut req: libc::ifreq = std::mem::zeroed();
                req.ifr_name[0] = b'l' as libc::c_char;
                req.ifr_name[1] = b'o' as libc::c_char;
                if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut req) == 0 {
                    req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
                    libc::ioctl(s, libc::SIOCSIFFLAGS, &mut req);
                }
                libc::close(s);
            }
        }
    }
    Ok(())
}

pub async fn run(spec: RootfsRun) -> Result<Vec<String>> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("running steps inside an image rootfs requires root (or a microVM backend)");
    }
    for d in [&spec.upper, &spec.work, &spec.merged] {
        std::fs::create_dir_all(d)?;
    }
    let lower: Vec<String> = spec.lower.iter().rev().map(|p| p.to_string_lossy().into_owned()).collect();
    let opts = format!(
        "lowerdir={},upperdir={},workdir={}",
        lower.join(":"),
        spec.upper.display(),
        spec.work.display()
    );
    let mut binds = Vec::new();
    for b in &spec.binds {
        let guest_rel = b.guest.trim_start_matches('/');
        let target = spec.upper.join(guest_rel);
        let is_file = b.host.is_file();
        if is_file {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !target.exists() {
                std::fs::write(&target, b"")?;
            }
        } else {
            std::fs::create_dir_all(&target)?;
        }
        binds.push((cpath(&b.host)?, cpath(&spec.merged.join(guest_rel))?, b.readonly));
    }
    for d in ["proc", "dev", "sys", "etc", "tmp"] {
        std::fs::create_dir_all(spec.upper.join(d))?;
    }
    let resolv = Path::new("/run/systemd/resolve/resolv.conf");
    let resolv_src = if spec.network {
        let src = if resolv.exists() { resolv.to_path_buf() } else { PathBuf::from("/etc/resolv.conf") };
        let t = spec.upper.join("etc/resolv.conf");
        if !t.exists() {
            std::fs::write(&t, b"")?;
        }
        Some(cpath(&src)?)
    } else {
        None
    };
    let prepared = Prepared {
        merged: cpath(&spec.merged)?,
        overlay_opts: CString::new(opts)?,
        binds,
        proc_target: cpath(&spec.merged.join("proc"))?,
        dev_target: cpath(&spec.merged.join("dev"))?,
        sys_target: cpath(&spec.merged.join("sys"))?,
        resolv_src,
        resolv_target: cpath(&spec.merged.join("etc/resolv.conf"))?,
        cwd: CString::new(spec.cwd.clone())?,
        network: spec.network,
    };
    let mut c = Command::new(&spec.argv[0]);
    c.args(&spec.argv[1..]).env_clear().envs(&spec.env);
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    unsafe {
        c.pre_exec(move || enter(&prepared));
    }
    let mut child = c.spawn().with_context(|| format!("starting {} in image rootfs", spec.argv[0]))?;
    let tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<String>::new()));
    let mut tasks = Vec::new();
    let streams: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> =
        vec![Box::new(child.stdout.take().unwrap()), Box::new(child.stderr.take().unwrap())];
    for s in streams {
        let tail = tail.clone();
        let step = spec.step.clone();
        tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                acro_events::log(&step, line.clone());
                let mut t = tail.lock().unwrap();
                t.push_back(line);
                if t.len() > 40 {
                    t.pop_front();
                }
            }
        }));
    }
    let status = child.wait().await?;
    for t in tasks {
        let _ = t.await;
    }
    let tail: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
    for b in &spec.binds {
        let target = spec.upper.join(b.guest.trim_start_matches('/'));
        if b.host.is_file() {
            let _ = std::fs::remove_file(&target);
        } else {
            let _ = std::fs::remove_dir(&target);
        }
    }
    if !status.success() {
        bail!("`{}` failed with {status}\n{}", spec.argv.join(" "), tail.join("\n"));
    }
    Ok(tail)
}
