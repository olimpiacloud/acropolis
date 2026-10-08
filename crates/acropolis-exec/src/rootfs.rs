use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
    dev_nodes: Vec<(CString, CString)>,
    dev_links: Vec<(CString, CString)>,
    dev_shm: CString,
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
    if rc != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn enter(p: &Prepared) -> std::io::Result<()> {
    unsafe {
        let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWPID;
        if !p.network {
            flags |= libc::CLONE_NEWNET;
        }
        check(libc::unshare(flags))?;
        let pid = libc::fork();
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid > 0 {
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
                for fd in 3..4096 {
                    libc::close(fd);
                }
            }
            let mut status: libc::c_int = 0;
            while libc::waitpid(pid, &mut status, 0) < 0 {
                if *libc::__errno_location() != libc::EINTR {
                    libc::_exit(70);
                }
            }
            if libc::WIFEXITED(status) {
                libc::_exit(libc::WEXITSTATUS(status));
            }
            libc::_exit(128 + libc::WTERMSIG(status));
        }
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        check(libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ))?;
        check(libc::mount(
            c"overlay".as_ptr(),
            p.merged.as_ptr(),
            c"overlay".as_ptr(),
            0,
            p.overlay_opts.as_ptr() as *const libc::c_void,
        ))?;
        check(libc::mount(
            c"proc".as_ptr(),
            p.proc_target.as_ptr(),
            c"proc".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            std::ptr::null(),
        ))?;
        check(libc::mount(
            c"tmpfs".as_ptr(),
            p.dev_target.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            c"mode=755,size=65536k".as_ptr() as *const libc::c_void,
        ))?;
        for (host, target) in &p.dev_nodes {
            let fd = libc::open(target.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC, 0o666);
            if fd >= 0 {
                libc::close(fd);
                check(libc::mount(
                    host.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                ))?;
            }
        }
        for (target, link) in &p.dev_links {
            libc::symlink(target.as_ptr(), link.as_ptr());
        }
        if libc::mkdir(p.dev_shm.as_ptr(), 0o1777) == 0 {
            let _ = libc::mount(
                c"tmpfs".as_ptr(),
                p.dev_shm.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"mode=1777".as_ptr() as *const libc::c_void,
            );
        }
        if libc::mount(
            c"/sys".as_ptr(),
            p.sys_target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) == 0
        {
            let _ = libc::mount(
                std::ptr::null(),
                p.sys_target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                std::ptr::null(),
            );
        }
        if let Some(src) = &p.resolv_src {
            let _ = libc::mount(
                src.as_ptr(),
                p.resolv_target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND,
                std::ptr::null(),
            );
        }
        for (host, guest, ro) in &p.binds {
            check(libc::mount(
                host.as_ptr(),
                guest.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            ))?;
            if *ro {
                check(libc::mount(
                    std::ptr::null(),
                    guest.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
                    std::ptr::null(),
                ))?;
            }
        }
        check(libc::chroot(p.merged.as_ptr()))?;
        check(libc::chdir(p.cwd.as_ptr()))?;
        crate::drop_dangerous_caps()?;
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
    let lower: Vec<String> = spec
        .lower
        .iter()
        .rev()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
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
    for d in ["proc", "dev", "sys", "etc"] {
        std::fs::create_dir_all(spec.upper.join(d))?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let tmp = spec.upper.join("tmp");
        std::fs::create_dir_all(&tmp)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o1777))?;
    }
    let resolv = Path::new("/run/systemd/resolve/resolv.conf");
    let resolv_src = if spec.network {
        let src = if resolv.exists() {
            resolv.to_path_buf()
        } else {
            PathBuf::from("/etc/resolv.conf")
        };
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
        dev_nodes: ["null", "zero", "full", "random", "urandom", "tty"]
            .iter()
            .map(|n| {
                Ok((
                    cpath(&Path::new("/dev").join(n))?,
                    cpath(&spec.merged.join("dev").join(n))?,
                ))
            })
            .collect::<Result<_>>()?,
        dev_links: [
            ("/proc/self/fd", "fd"),
            ("/proc/self/fd/0", "stdin"),
            ("/proc/self/fd/1", "stdout"),
            ("/proc/self/fd/2", "stderr"),
            ("pts/ptmx", "ptmx"),
        ]
        .iter()
        .map(|(t, l)| Ok((CString::new(*t)?, cpath(&spec.merged.join("dev").join(l))?)))
        .collect::<Result<_>>()?,
        dev_shm: cpath(&spec.merged.join("dev/shm"))?,
        sys_target: cpath(&spec.merged.join("sys"))?,
        resolv_src,
        resolv_target: cpath(&spec.merged.join("etc/resolv.conf"))?,
        cwd: CString::new(spec.cwd.clone())?,
        network: spec.network,
    };
    let mut c = Command::new(&spec.argv[0]);
    c.args(&spec.argv[1..]).env_clear().envs(&spec.env);
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    unsafe {
        c.pre_exec(move || enter(&prepared));
    }
    let mut child = c
        .spawn()
        .with_context(|| format!("starting {} in image rootfs", spec.argv[0]))?;
    let group = crate::ProcessGroup::of(&child);
    let (status, tail) = crate::wait_with_output(&spec.step, &mut child, group).await?;
    for b in &spec.binds {
        let target = spec.upper.join(b.guest.trim_start_matches('/'));
        if b.host.is_file() {
            let _ = std::fs::remove_file(&target);
        } else {
            let _ = std::fs::remove_dir(&target);
        }
    }
    if !status.success() {
        return Err(crate::CommandFailed::new(&spec.argv, status, tail).into());
    }
    Ok(tail)
}
