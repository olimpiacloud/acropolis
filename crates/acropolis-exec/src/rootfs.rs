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
    sys_target: CString,
    resolv_src: Option<CString>,
    resolv_target: CString,
    cwd: CString,
    network: bool,
}

fn cpath(p: &Path) -> Result<CString> {
    Ok(CString::new(p.as_os_str().as_bytes())?)
}

fn enter(p: &Prepared) -> std::io::Result<()> {
    use crate::check;
    unsafe {
        let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC | libc::CLONE_NEWPID;
        if !p.network {
            flags |= libc::CLONE_NEWNET;
        }
        check(libc::unshare(flags))?;
        crate::fork_into_pid_namespace()?;
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
        crate::bind_readonly(c"/sys", &p.sys_target)?;
        if let Some(src) = &p.resolv_src {
            let _ = crate::bind_readonly(src, &p.resolv_target);
        }
        for (host, guest, ro) in &p.binds {
            if *ro {
                crate::bind_readonly(host, guest)?;
            } else {
                check(libc::mount(
                    host.as_ptr(),
                    guest.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ))?;
            }
        }
        check(libc::chroot(p.merged.as_ptr()))?;
        check(libc::chdir(p.cwd.as_ptr()))?;
        crate::mount_proc_and_dev()?;
        if !p.network {
            crate::bring_up_lo();
        }
        crate::drop_caps()
    }
}

const OVERLAYFS_SUPER_MAGIC: libc::c_long = 0x794c_7630;

fn on_overlayfs(dir: &Path) -> Result<bool> {
    let path = cpath(dir)?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    crate::check(unsafe { libc::statfs(path.as_ptr(), &mut st) })?;
    Ok(st.f_type as libc::c_long == OVERLAYFS_SUPER_MAGIC)
}

pub async fn run(spec: RootfsRun) -> Result<Vec<String>> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("running steps inside an image rootfs requires root (or a microVM backend)");
    }
    for d in [&spec.upper, &spec.work, &spec.merged] {
        std::fs::create_dir_all(d)?;
    }
    // The kernel refuses an overlayfs upper dir on overlayfs (EINVAL), which is what a container's root is.
    if on_overlayfs(&spec.upper)? {
        bail!(
            "{} is on overlayfs, which can't hold the upper dir of an image step; mount a volume (ext4, xfs or tmpfs) at $ACROPOLIS_HOME/work",
            spec.upper.display()
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rootfs_steps_are_isolated_and_keep_loopback() {
        let busybox = Path::new("/usr/bin/busybox");
        if unsafe { libc::geteuid() } != 0 || !busybox.exists() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("acropolis-rootfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let lower = tmp.join("lower");
        std::fs::create_dir_all(lower.join("bin")).unwrap();
        std::fs::copy(busybox, lower.join("bin/busybox")).unwrap();
        let checks = [
            "busybox test $$ -eq 1",
            "busybox ip link show lo | busybox grep -q ',UP'",
            "busybox grep -q '^CapBnd:[[:space:]]*00000000000004fb$' /proc/self/status",
            "busybox test -c /dev/null && busybox test -z \"$(busybox find /dev -type b)\"",
            "if echo x > /proc/sys/kernel/hostname; then exit 1; fi",
            "if busybox mkdir /sys/fs/cgroup/acropolis-probe; then busybox rmdir /sys/fs/cgroup/acropolis-probe; exit 1; fi",
        ];
        for (i, c) in checks.iter().enumerate() {
            let step = tmp.join(i.to_string());
            let spec = RootfsRun {
                step: "test".into(),
                lower: vec![lower.clone()],
                upper: step.join("upper"),
                work: step.join("work"),
                merged: step.join("merged"),
                binds: Vec::new(),
                argv: vec!["/bin/busybox".into(), "sh".into(), "-c".into(), c.to_string()],
                env: [("PATH".to_string(), "/bin".to_string())].into_iter().collect(),
                cwd: "/".into(),
                network: false,
            };
            let r = run(spec).await;
            assert!(r.is_ok(), "{c}: {r:?}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
