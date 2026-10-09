pub mod rootfs;
mod seccomp;
use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::BufReader;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct Cmd {
    pub step: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub network: bool,
    /// Directories a hardened step may write. When set, everything else is read-only for the step
    /// (besides /tmp and /var/tmp): the builder's binaries, /etc and other apps' caches included.
    pub writable: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Output {
    pub tail: Vec<String>,
}

#[derive(Debug)]
pub struct CommandFailed {
    pub command: String,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub status: String,
    pub tail: Vec<String>,
}

impl std::fmt::Display for CommandFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` failed with {}\n{}",
            self.command,
            self.status,
            self.tail.join("\n")
        )
    }
}

impl std::error::Error for CommandFailed {}

impl CommandFailed {
    pub fn new(argv: &[String], status: std::process::ExitStatus, tail: Vec<String>) -> Self {
        use std::os::unix::process::ExitStatusExt;
        CommandFailed {
            command: argv.join(" "),
            code: status.code(),
            signal: status.signal(),
            status: status.to_string(),
            tail,
        }
    }

    pub fn killed(&self) -> bool {
        self.signal == Some(libc::SIGKILL) || self.code == Some(137)
    }
}

const MAX_LINE: usize = 64 * 1024;
const MAX_STEP_LOG: u64 = 32 * 1024 * 1024;

pub async fn collect_output<R: tokio::io::AsyncRead + Unpin>(
    step: &str,
    reader: R,
    tail: Arc<Mutex<VecDeque<String>>>,
) {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut logged: u64 = 0;
    let mut truncated = false;
    let emit = |bytes: &[u8], logged: &mut u64, truncated: &mut bool| {
        let text = String::from_utf8_lossy(bytes).into_owned();
        *logged += text.len() as u64 + 1;
        if *logged <= MAX_STEP_LOG {
            acropolis_events::log(step, text.clone());
        } else if !*truncated {
            *truncated = true;
            acropolis_events::log(
                step,
                format!("[acropolis] log truncated after {} MB", MAX_STEP_LOG >> 20),
            );
        }
        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
        t.push_back(text);
        if t.len() > 40 {
            t.pop_front();
        }
    };
    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' {
                emit(&line, &mut logged, &mut truncated);
                line.clear();
            } else {
                line.push(b);
                if line.len() >= MAX_LINE {
                    emit(&line, &mut logged, &mut truncated);
                    line.clear();
                }
            }
        }
    }
    if !line.is_empty() {
        emit(&line, &mut logged, &mut truncated);
    }
}

pub async fn wait_with_output(
    step: &str,
    child: &mut tokio::process::Child,
    group: ProcessGroup,
) -> Result<(std::process::ExitStatus, Vec<String>)> {
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut tasks = Vec::new();
    if let Some(out) = child.stdout.take() {
        let (t, s) = (tail.clone(), step.to_string());
        tasks.push(tokio::spawn(async move { collect_output(&s, out, t).await }));
    }
    if let Some(err) = child.stderr.take() {
        let (t, s) = (tail.clone(), step.to_string());
        tasks.push(tokio::spawn(async move { collect_output(&s, err, t).await }));
    }
    let status = child.wait().await?;
    drop(group);
    for t in tasks {
        let abort = t.abort_handle();
        if tokio::time::timeout(std::time::Duration::from_secs(2), t)
            .await
            .is_err()
        {
            abort.abort();
        }
    }
    let tail: Vec<String> = tail.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
    Ok((status, tail))
}

pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    fn hermetic(&self) -> bool;
    fn run(&self, cmd: Cmd) -> BoxFuture<'_, Result<Output>>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    None,
    NetNamespace,
}

pub struct HostExecutor {
    pub isolation: Isolation,
    pub readonly: Vec<std::path::PathBuf>,
}

impl HostExecutor {
    pub fn detect() -> Self {
        if probe_netns() {
            HostExecutor {
                isolation: Isolation::NetNamespace,
                readonly: Vec::new(),
            }
        } else {
            HostExecutor {
                isolation: Isolation::None,
                readonly: Vec::new(),
            }
        }
    }

    pub fn with_readonly(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.readonly = paths;
        self
    }
}

/// CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID, SETUID, NET_BIND_SERVICE: what package
/// managers and build scripts use as root. Every other capability is removed from all sets,
/// including ones the kernel adds later.
const KEEP_CAPS: &[u32] = &[0, 1, 3, 4, 5, 6, 7, 10];

pub(crate) fn check(rc: libc::c_int) -> std::io::Result<()> {
    if rc != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) unsafe fn drop_caps() -> std::io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let keep = KEEP_CAPS.iter().fold(0u64, |m, &c| m | (1 << c));
    unsafe {
        for c in 0..64u32 {
            if keep & (1 << c) == 0 && libc::prctl(libc::PR_CAPBSET_DROP, c as libc::c_ulong, 0, 0, 0) != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(e);
            }
        }
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0,
            0,
            0,
        );
        let mut hdr = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let mut data = [Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];
        if libc::syscall(libc::SYS_capget, &mut hdr as *mut Header, data.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for (d, mask) in data.iter_mut().zip([keep as u32, (keep >> 32) as u32]) {
            d.effective &= mask;
            d.permitted &= mask;
            d.inheritable &= mask;
        }
        if libc::syscall(libc::SYS_capset, &mut hdr as *mut Header, data.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
    }
}

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 1;

/// Bind-mounts `src` on `target` read-only, submounts included: a plain read-only remount only
/// covers the top mount, which left e.g. /sys/fs/cgroup writable under a read-only /sys.
pub(crate) unsafe fn bind_readonly(src: &CStr, target: &CStr) -> std::io::Result<()> {
    unsafe {
        bind(src, target)?;
        set_readonly(target, true)
    }
}

/// Bind-mounts `path` on itself writable, on top of a read-only tree (a bind inherits the read-only flag).
pub(crate) unsafe fn bind_writable(path: &CStr) -> std::io::Result<()> {
    unsafe {
        bind(path, path)?;
        set_readonly(path, false)
    }
}

unsafe fn bind(src: &CStr, target: &CStr) -> std::io::Result<()> {
    unsafe {
        check(libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ))
    }
}

/// Sets or clears the read-only flag of the mount at `target` and all its submounts.
/// Kernels before 5.12 (no `mount_setattr`) only change the top mount.
unsafe fn set_readonly(target: &CStr, readonly: bool) -> std::io::Result<()> {
    unsafe {
        let attr = MountAttr {
            attr_set: if readonly { MOUNT_ATTR_RDONLY } else { 0 },
            attr_clr: if readonly { 0 } else { MOUNT_ATTR_RDONLY },
            propagation: 0,
            userns_fd: 0,
        };
        if libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::AT_RECURSIVE as libc::c_uint,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        ) == 0
        {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOSYS) {
            return Err(e);
        }
        let flags = libc::MS_BIND | libc::MS_REMOUNT | if readonly { libc::MS_RDONLY } else { 0 };
        check(libc::mount(
            std::ptr::null(),
            target.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        ))
    }
}

/// Docker's read-only /proc paths: root can write sysctls without any capability, and
/// kernel.core_pattern or kernel.modprobe run a helper as host root.
const PROC_READONLY: &[&CStr] = &[
    c"/proc/sys",
    c"/proc/sysrq-trigger",
    c"/proc/irq",
    c"/proc/bus",
    c"/proc/fs",
];
const DEV_NODES: &[(&CStr, u32, u32)] = &[
    (c"/dev/null", 1, 3),
    (c"/dev/zero", 1, 5),
    (c"/dev/full", 1, 7),
    (c"/dev/random", 1, 8),
    (c"/dev/urandom", 1, 9),
    (c"/dev/tty", 5, 0),
];
const DEV_LINKS: &[(&CStr, &CStr)] = &[
    (c"/proc/self/fd", c"/dev/fd"),
    (c"/proc/self/fd/0", c"/dev/stdin"),
    (c"/proc/self/fd/1", c"/dev/stdout"),
    (c"/proc/self/fd/2", c"/dev/stderr"),
    (c"pts/ptmx", c"/dev/ptmx"),
];

/// Mounts, on the current root, a /proc of the step's PID namespace and a /dev with only the
/// standard character devices (the host /dev of a privileged container has its disks).
pub(crate) unsafe fn mount_proc_and_dev() -> std::io::Result<()> {
    unsafe {
        check(libc::mount(
            c"proc".as_ptr(),
            c"/proc".as_ptr(),
            c"proc".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            std::ptr::null(),
        ))?;
        for p in PROC_READONLY {
            match bind_readonly(p, p) {
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
                r => r?,
            }
        }
        check(libc::mount(
            c"tmpfs".as_ptr(),
            c"/dev".as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NOEXEC,
            c"mode=755,size=65536k".as_ptr() as *const libc::c_void,
        ))?;
        for &(path, major, minor) in DEV_NODES {
            check(libc::mknod(
                path.as_ptr(),
                libc::S_IFCHR | 0o666,
                libc::makedev(major, minor),
            ))?;
            check(libc::chmod(path.as_ptr(), 0o666))?;
        }
        for &(target, link) in DEV_LINKS {
            libc::symlink(target.as_ptr(), link.as_ptr());
        }
        if libc::mkdir(c"/dev/shm".as_ptr(), 0o1777) == 0 {
            let _ = libc::mount(
                c"tmpfs".as_ptr(),
                c"/dev/shm".as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"mode=1777".as_ptr() as *const libc::c_void,
            );
        }
    }
    Ok(())
}

/// After `unshare(CLONE_NEWPID)` the caller is still outside the new namespace: fork its PID 1.
/// The calling copy only waits and exits with the child's status, so this returns in the child.
pub(crate) unsafe fn fork_into_pid_namespace() -> std::io::Result<()> {
    unsafe {
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
    }
    Ok(())
}

pub(crate) const OFFLINE_RESOLV: &str = "nameserver 192.0.2.1\noptions timeout:1 attempts:1\n";

pub(crate) fn offline_resolv_conf() -> Option<CString> {
    let path = std::env::temp_dir().join(format!("acropolis-offline-resolv-{}.conf", unsafe { libc::geteuid() }));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(OFFLINE_RESOLV) {
        use std::io::Write;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::fs::File::create_new(&tmp)
            .ok()?
            .write_all(OFFLINE_RESOLV.as_bytes())
            .ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

/// Docker credential directories of this process, hidden from hardened steps: they run as root
/// and could otherwise read the passwords acropolis pushes with.
fn credential_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        std::env::var_os("DOCKER_CONFIG").map(PathBuf::from),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker")),
    ]
    .into_iter()
    .flatten()
    .chain([PathBuf::from("/root/.docker")])
    .filter_map(|d| std::path::absolute(d).ok())
    .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Steps of a root acropolis: own mount and PID namespaces (the step cannot see acropolis or
/// read its environment, which holds the registry credentials), read-only store, toolchains,
/// /sys and sysctls, hidden `masked` directories, a /dev without host devices and only the
/// capabilities in `KEEP_CAPS`, under the `seccomp` filter. With `writable` set, the whole tree
/// is read-only except /tmp, /var/tmp (`scratch`) and those directories; `readonly` still wins
/// over scratch dirs.
fn enter_hardened(
    readonly: &[CString],
    writable: &[CString],
    scratch: &[CString],
    masked: &[CString],
    network: bool,
    resolv: Option<&CStr>,
    cwd: &CStr,
) -> std::io::Result<()> {
    unsafe {
        let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWPID;
        if !network {
            flags |= libc::CLONE_NEWNET;
        }
        check(libc::unshare(flags))?;
        fork_into_pid_namespace()?;
        check(libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ))?;
        if !writable.is_empty() {
            set_readonly(c"/", true)?;
            for p in scratch {
                bind_writable(p)?;
            }
        }
        if !network && let Some(r) = resolv {
            let _ = bind_readonly(r, c"/etc/resolv.conf");
        }
        for p in readonly {
            bind_readonly(p, p)?;
        }
        for p in writable {
            bind_writable(p)?;
        }
        for p in masked {
            check(libc::mount(
                c"tmpfs".as_ptr(),
                p.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                c"size=4k,mode=700".as_ptr() as *const libc::c_void,
            ))?;
        }
        bind_readonly(c"/sys", c"/sys")?;
        mount_proc_and_dev()?;
        // The working directory was entered before these mounts and still points at the mount
        // underneath: enter it again so relative paths see the writable binds.
        check(libc::chdir(cwd.as_ptr()))?;
        if !network {
            bring_up_lo();
        }
        drop_caps()?;
        seccomp::install()
    }
}

fn probe_netns() -> bool {
    let mut cmd = std::process::Command::new("/bin/true");
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(enter_netns);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

fn write_file(path: &[u8], data: &[u8]) -> bool {
    unsafe {
        let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return false;
        }
        let n = libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        libc::close(fd);
        n == data.len() as isize
    }
}

/// Needs CAP_NET_ADMIN: call before `drop_caps`.
pub(crate) fn bring_up_lo() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 {
            return;
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        let name = b"lo\0";
        for (i, b) in name.iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut req) == 0 {
            req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(s, libc::SIOCSIFFLAGS, &mut req);
        }
        libc::close(s);
    }
}

fn enter_netns() -> std::io::Result<()> {
    unsafe {
        if libc::unshare(libc::CLONE_NEWNET) == 0 {
            bring_up_lo();
            return Ok(());
        }
        let uid = libc::getuid();
        let gid = libc::getgid();
        if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = [0u8; 64];
        write_file(b"/proc/self/setgroups\0", b"deny");
        let n = fmt_map(&mut buf, uid);
        if !write_file(b"/proc/self/uid_map\0", &buf[..n]) {
            return Err(std::io::Error::other("uid_map"));
        }
        let n = fmt_map(&mut buf, gid);
        if !write_file(b"/proc/self/gid_map\0", &buf[..n]) {
            return Err(std::io::Error::other("gid_map"));
        }
        bring_up_lo();
        Ok(())
    }
}

fn fmt_map(buf: &mut [u8; 64], id: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut n = 0;
    let mut v = id;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        n += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let mut pos = 0;
    for _ in 0..2 {
        for i in (0..n).rev() {
            buf[pos] = digits[i];
            pos += 1;
        }
        buf[pos] = b' ';
        pos += 1;
    }
    buf[pos] = b'1';
    buf[pos + 1] = b'\n';
    pos + 2
}

pub struct ProcessGroup(Option<i32>);

impl ProcessGroup {
    pub fn of(child: &tokio::process::Child) -> Self {
        ProcessGroup(child.id().map(|p| p as i32))
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0
            && pgid > 1
        {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

impl Executor for HostExecutor {
    fn name(&self) -> &'static str {
        match self.isolation {
            Isolation::None => "host",
            Isolation::NetNamespace => "host+netns",
        }
    }

    fn hermetic(&self) -> bool {
        self.isolation == Isolation::NetNamespace
    }

    fn run(&self, cmd: Cmd) -> BoxFuture<'_, Result<Output>> {
        Box::pin(self.run_masked(cmd, credential_dirs()))
    }
}

impl HostExecutor {
    async fn run_masked(&self, cmd: Cmd, masked: Vec<PathBuf>) -> Result<Output> {
        if cmd.argv.is_empty() {
            bail!("empty command");
        }
        let mut c = Command::new(&cmd.argv[0]);
        c.args(&cmd.argv[1..]).current_dir(&cmd.cwd).env_clear().envs(&cmd.env);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c.kill_on_drop(true);
        c.process_group(0);
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root && self.isolation == Isolation::NetNamespace {
            let cpaths = |paths: &[PathBuf], keep: fn(&std::path::Path) -> bool| -> Vec<CString> {
                paths
                    .iter()
                    .filter(|p| keep(p))
                    .filter_map(|p| CString::new(p.as_os_str().as_encoded_bytes()).ok())
                    .collect()
            };
            let ro = cpaths(&self.readonly, |p| p.exists());
            for w in &cmd.writable {
                std::fs::create_dir_all(w).with_context(|| format!("creating {}", w.display()))?;
            }
            let writable: Vec<PathBuf> = cmd
                .writable
                .iter()
                .map(std::path::absolute)
                .collect::<std::io::Result<_>>()?;
            let writable = cpaths(&writable, |_| true);
            let cwd = CString::new(std::path::absolute(&cmd.cwd)?.into_os_string().into_encoded_bytes())?;
            let scratch = cpaths(&[PathBuf::from("/tmp"), PathBuf::from("/var/tmp")], |p| p.is_dir());
            let masked = cpaths(&masked, |p| p.is_dir());
            let network = cmd.network;
            let resolv = if network { None } else { offline_resolv_conf() };
            unsafe {
                c.pre_exec(move || enter_hardened(&ro, &writable, &scratch, &masked, network, resolv.as_deref(), &cwd));
            }
        } else if !cmd.network {
            if self.isolation != Isolation::NetNamespace {
                bail!(
                    "step {} must run without network but this host cannot create a network namespace; pass --hermetic=off to allow it",
                    cmd.step
                );
            }
            unsafe {
                c.pre_exec(enter_netns);
            }
        }
        let mut child = c
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning {}: {e}", cmd.argv[0]))?;
        let group = ProcessGroup::of(&child);
        let (status, tail) = wait_with_output(&cmd.step, &mut child, group).await?;
        if !status.success() {
            return Err(CommandFailed::new(&cmd.argv, status, tail).into());
        }
        Ok(Output { tail })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_format() {
        let mut b = [0u8; 64];
        let n = fmt_map(&mut b, 1000);
        assert_eq!(&b[..n], b"1000 1000 1\n");
    }

    fn sh(script: &str, dir: &std::path::Path) -> Cmd {
        Cmd {
            step: "test".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: dir.to_path_buf(),
            env: [("PATH".to_string(), "/usr/bin:/bin".to_string())]
                .into_iter()
                .collect(),
            network: false,
            writable: Vec::new(),
        }
    }

    #[tokio::test]
    async fn readonly_paths_cannot_be_written_or_remounted() {
        if unsafe { libc::geteuid() } != 0 || !probe_netns() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("acropolis-exec-ro-{}", std::process::id()));
        let store = tmp.join("store");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join("blob"), b"original").unwrap();
        let e = HostExecutor::detect().with_readonly(vec![store.clone()]);
        let attempts = [
            format!("echo poisoned > {}/blob", store.display()),
            format!(
                "mount -o remount,rw {} && echo poisoned > {}/blob",
                store.display(),
                store.display()
            ),
            format!("umount {} && echo poisoned > {}/blob", store.display(), store.display()),
        ];
        for a in &attempts {
            assert!(e.run(sh(a, &tmp)).await.is_err(), "{a} should fail");
        }
        assert_eq!(std::fs::read(store.join("blob")).unwrap(), b"original");
        assert!(e.run(sh("echo ok > out", &tmp)).await.is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn hardened_steps_cannot_reach_acropolis_or_the_host_kernel() {
        if unsafe { libc::geteuid() } != 0 || !probe_netns() {
            return;
        }
        let tmp = std::env::temp_dir();
        let e = HostExecutor::detect();
        let checks = [
            "test $$ -eq 1".to_string(),
            format!("test ! -e /proc/{}/environ", std::process::id()),
            "grep -q '^CapBnd:[[:space:]]*00000000000004fb$' /proc/self/status".to_string(),
            "grep -q '^CapEff:[[:space:]]*00000000000004fb$' /proc/self/status".to_string(),
            "test -c /dev/null && echo ok > /dev/null && test -z \"$(find /dev -type b)\"".to_string(),
            "if cat /proc/sys/kernel/hostname > /proc/sys/kernel/hostname; then exit 1; fi".to_string(),
            "if mkdir /sys/fs/cgroup/acropolis-probe; then rmdir /sys/fs/cgroup/acropolis-probe; exit 1; fi"
                .to_string(),
            "grep -q '^Seccomp:[[:space:]]*2$' /proc/self/status".to_string(),
            "if unshare -U true; then exit 1; fi".to_string(),
            "if unshare -r true; then exit 1; fi".to_string(),
            "if unshare -m true; then exit 1; fi".to_string(),
            "if mount -t tmpfs x /mnt; then exit 1; fi".to_string(),
            "echo ok; ls / > /dev/null".to_string(),
        ];
        for c in &checks {
            let r = e.run(sh(c, &tmp)).await;
            assert!(r.is_ok(), "{c}: {r:?}");
        }
    }

    #[tokio::test]
    async fn credential_dirs_are_hidden_from_hardened_steps() {
        if unsafe { libc::geteuid() } != 0 || !probe_netns() {
            return;
        }
        let creds = std::env::temp_dir().join(format!("acropolis-exec-creds-{}", std::process::id()));
        std::fs::create_dir_all(&creds).unwrap();
        std::fs::write(creds.join("config.json"), b"{\"auths\":{}}").unwrap();
        let e = HostExecutor::detect();
        let tmp = std::env::temp_dir();
        let read = format!("cat {}/config.json", creds.display());
        assert!(e.run_masked(sh(&read, &tmp), Vec::new()).await.is_ok());
        assert!(e.run_masked(sh(&read, &tmp), vec![creds.clone()]).await.is_err());
        let write = format!("touch {}/x", creds.display());
        assert!(e.run_masked(sh(&write, &tmp), vec![creds.clone()]).await.is_err());
        let _ = std::fs::remove_dir_all(&creds);
    }

    #[tokio::test]
    async fn steps_only_write_their_own_directories() {
        if unsafe { libc::geteuid() } != 0 || !probe_netns() {
            return;
        }
        let home = std::env::temp_dir().join(format!("acropolis-exec-home-{}", std::process::id()));
        let own = home.join("work/this-build");
        let other = home.join("cache/apps/other-app");
        std::fs::create_dir_all(&other).unwrap();
        let e = HostExecutor::detect().with_readonly(vec![home.clone()]);
        let run = |script: String| {
            let mut c = sh(&script, &own);
            c.writable = vec![own.clone()];
            e.run_masked(c, Vec::new())
        };
        assert!(run(format!("touch {}/out", own.display())).await.is_ok());
        assert!(run("mkdir -p build && touch build/Makefile".into()).await.is_ok());
        assert!(
            run("touch /tmp/acropolis-exec-scratch && rm /tmp/acropolis-exec-scratch".into())
                .await
                .is_ok()
        );
        assert!(run(format!("touch {}/poison", other.display())).await.is_err());
        assert!(run("touch /usr/acropolis-exec-probe".into()).await.is_err());
        assert!(
            run("echo nameserver 192.0.2.1 > /etc/resolv.conf".into())
                .await
                .is_err()
        );
        assert!(!other.join("poison").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn steps_do_not_inherit_the_acropolis_environment() {
        let e = HostExecutor {
            isolation: Isolation::None,
            readonly: Vec::new(),
        };
        let mut c = sh(
            "test -z \"${HOME-}\" && test -z \"${CARGO-}\" && test \"$PATH\" = /usr/bin:/bin",
            &std::env::temp_dir(),
        );
        c.network = true;
        assert!(std::env::var_os("HOME").is_some() || std::env::var_os("CARGO").is_some());
        assert!(e.run(c).await.is_ok());
    }

    #[tokio::test]
    async fn background_children_do_not_hang_the_step() {
        let tmp = std::env::temp_dir();
        let e = HostExecutor {
            isolation: Isolation::None,
            readonly: Vec::new(),
        };
        let started = std::time::Instant::now();
        let mut c = sh("sleep 600 & echo ok", &tmp);
        c.network = true;
        let out = e.run(c).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(out.tail, vec!["ok".to_string()]);
    }

    #[tokio::test]
    async fn huge_lines_are_split_and_failures_are_typed() {
        let tmp = std::env::temp_dir();
        let e = HostExecutor {
            isolation: Isolation::None,
            readonly: Vec::new(),
        };
        let mut c = sh("head -c 1000000 /dev/zero | tr '\\0' x; echo; exit 3", &tmp);
        c.network = true;
        let err = e.run(c).await.unwrap_err();
        let cf = err.downcast_ref::<CommandFailed>().unwrap();
        assert_eq!(cf.code, Some(3));
        assert!(cf.tail.iter().all(|l| l.len() <= MAX_LINE));
        assert!(cf.tail.len() >= 15);
    }

    #[tokio::test]
    async fn dropping_a_run_kills_the_whole_process_group() {
        let tmp = std::env::temp_dir().join(format!("acropolis-exec-pg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let marker = format!("acropolis-pg-test-{}", std::process::id());
        let e = HostExecutor {
            isolation: Isolation::None,
            readonly: Vec::new(),
        };
        let mut cmd = sh(&format!("sh -c 'sleep 300; echo {marker}' & sleep 300"), &tmp);
        cmd.network = true;
        let fut = e.run(cmd);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), fut).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let ps = std::process::Command::new("pgrep")
            .args(["-f", &marker])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&ps.stdout).trim().is_empty(),
            "orphaned processes survived"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
