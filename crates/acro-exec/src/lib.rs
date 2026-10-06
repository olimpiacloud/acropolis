pub mod rootfs;
use anyhow::{Result, bail};
use futures::future::BoxFuture;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
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
}

impl HostExecutor {
    pub fn detect() -> Self {
        if probe_netns() {
            HostExecutor { isolation: Isolation::NetNamespace }
        } else {
            HostExecutor { isolation: Isolation::None }
        }
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
            if !cmd.network {
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
            let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
            let mut tasks = Vec::new();
            let streams: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = vec![
                Box::new(child.stdout.take().unwrap()),
                Box::new(child.stderr.take().unwrap()),
            ];
            for s in streams {
                let tail = tail.clone();
                let step = cmd.step.clone();
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
            if !status.success() {
                bail!("`{}` failed with {status}\n{}", cmd.argv.join(" "), tail.join("\n"));
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
}
