pub mod rootfs;
use anyhow::{Result, bail};
use futures::future::BoxFuture;
use std::collections::{BTreeMap, VecDeque};
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
        write!(f, "`{}` failed with {}\n{}", self.command, self.status, self.tail.join("\n"))
    }
}

impl std::error::Error for CommandFailed {}

impl CommandFailed {
    pub fn new(argv: &[String], status: std::process::ExitStatus, tail: Vec<String>) -> Self {
        use std::os::unix::process::ExitStatusExt;
        CommandFailed { command: argv.join(" "), code: status.code(), signal: status.signal(), status: status.to_string(), tail }
    }

    pub fn killed(&self) -> bool {
        self.signal == Some(libc::SIGKILL) || self.code == Some(137)
    }
}

const MAX_LINE: usize = 64 * 1024;
const MAX_STEP_LOG: u64 = 32 * 1024 * 1024;

pub async fn collect_output<R: tokio::io::AsyncRead + Unpin>(step: &str, reader: R, tail: Arc<Mutex<VecDeque<String>>>) {
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
            acropolis_events::log(step, format!("[acropolis] log truncated after {} MB", MAX_STEP_LOG >> 20));
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

pub async fn wait_with_output(step: &str, child: &mut tokio::process::Child, group: ProcessGroup) -> Result<(std::process::ExitStatus, Vec<String>)> {
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
        if tokio::time::timeout(std::time::Duration::from_secs(2), t).await.is_err() {
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
            HostExecutor { isolation: Isolation::NetNamespace, readonly: Vec::new() }
        } else {
            HostExecutor { isolation: Isolation::None, readonly: Vec::new() }
        }
    }

    pub fn with_readonly(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.readonly = paths;
        self
    }
}

pub(crate) const DROP_CAPS: &[u32] = &[9, 12, 16, 17, 18, 19, 20, 21, 22, 25, 27, 29, 30, 31, 32, 33, 34, 37, 38, 39];

pub(crate) unsafe fn drop_dangerous_caps() -> std::io::Result<()> {
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
    unsafe {
        for &c in DROP_CAPS {
            libc::prctl(libc::PR_CAPBSET_DROP, c as libc::c_ulong, 0, 0, 0);
        }
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0);
        let mut hdr = Header { version: 0x2008_0522, pid: 0 };
        let mut data = [Data { effective: 0, permitted: 0, inheritable: 0 }; 2];
        if libc::syscall(libc::SYS_capget, &mut hdr as *mut Header, data.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for &c in DROP_CAPS {
            let (i, bit) = ((c / 32) as usize, 1u32 << (c % 32));
            data[i].effective &= !bit;
            data[i].permitted &= !bit;
            data[i].inheritable &= !bit;
        }
        if libc::syscall(libc::SYS_capset, &mut hdr as *mut Header, data.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    }
    Ok(())
}

pub(crate) unsafe fn bind_readonly(path: &std::ffi::CStr) -> std::io::Result<()> {
    unsafe {
        if libc::mount(path.as_ptr(), path.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::mount(
            std::ptr::null(),
            path.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY,
            std::ptr::null(),
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) const OFFLINE_RESOLV: &str = "nameserver 192.0.2.1\noptions timeout:1 attempts:1\n";

pub(crate) fn offline_resolv_conf() -> Option<std::ffi::CString> {
    let path = std::env::temp_dir().join(format!("acropolis-offline-resolv-{}.conf", unsafe { libc::geteuid() }));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(OFFLINE_RESOLV) {
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, OFFLINE_RESOLV).ok()?;
        std::fs::rename(&tmp, &path).ok()?;
    }
    std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

fn enter_hardened(readonly: &[std::ffi::CString], network: bool, resolv: Option<&std::ffi::CStr>) -> std::io::Result<()> {
    unsafe {
        let mut flags = libc::CLONE_NEWNS;
        if !network {
            flags |= libc::CLONE_NEWNET;
        }
        if libc::unshare(flags) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if !network && let Some(r) = resolv {
            libc::mount(r.as_ptr(), c"/etc/resolv.conf".as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null());
        }
        for p in readonly {
            bind_readonly(p)?;
        }
        if !network {
            bring_up_lo();
        }
        drop_dangerous_caps()
    }
}

fn probe_netns() -> bool {
    let mut cmd = std::process::Command::new("/bin/true");
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| enter_netns());
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    matches!(cmd.status(), Ok(s) if s.success())
}

fn write_file(path: &[u8], data: &[u8]) -> bool {
    unsafe {
        let fd = libc::open(path.as_ptr() as *const libc::c_char, libc::O_WRONLY);
        if fd < 0 {
            return false;
        }
        let n = libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        libc::close(fd);
        n == data.len() as isize
    }
}

fn bring_up_lo() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
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
        Box::pin(async move {
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
                let ro: Vec<std::ffi::CString> = self
                    .readonly
                    .iter()
                    .filter(|p| p.exists())
                    .filter_map(|p| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok())
                    .collect();
                let network = cmd.network;
                let resolv = if network { None } else { offline_resolv_conf() };
                unsafe {
                    c.pre_exec(move || enter_hardened(&ro, network, resolv.as_deref()));
                }
            } else if !cmd.network {
                if self.isolation != Isolation::NetNamespace {
                    bail!(
                        "step {} must run without network but this host cannot create a network namespace; pass --hermetic=off to allow it",
                        cmd.step
                    );
                }
                unsafe {
                    c.pre_exec(|| enter_netns());
                }
            }
            let mut child = c.spawn().map_err(|e| anyhow::anyhow!("spawning {}: {e}", cmd.argv[0]))?;
            let group = ProcessGroup::of(&child);
            let (status, tail) = wait_with_output(&cmd.step, &mut child, group).await?;
            if !status.success() {
                return Err(CommandFailed::new(&cmd.argv, status, tail).into());
            }
            Ok(Output { tail })
        })
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
            env: [("PATH".to_string(), "/usr/bin:/bin".to_string())].into_iter().collect(),
            network: false,
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
            format!("mount -o remount,rw {} && echo poisoned > {}/blob", store.display(), store.display()),
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
    async fn background_children_do_not_hang_the_step() {
        let tmp = std::env::temp_dir();
        let e = HostExecutor { isolation: Isolation::None, readonly: Vec::new() };
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
        let e = HostExecutor { isolation: Isolation::None, readonly: Vec::new() };
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
        let e = HostExecutor { isolation: Isolation::None, readonly: Vec::new() };
        let mut cmd = sh(&format!("sh -c 'sleep 300; echo {marker}' & sleep 300"), &tmp);
        cmd.network = true;
        let fut = e.run(cmd);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), fut).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let ps = std::process::Command::new("pgrep").args(["-f", &marker]).output().unwrap();
        assert!(String::from_utf8_lossy(&ps.stdout).trim().is_empty(), "orphaned processes survived");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
