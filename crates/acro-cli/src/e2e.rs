use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StrOrVec {
    One(String),
    Many(Vec<String>),
}

impl StrOrVec {
    fn list(&self) -> Vec<String> {
        match self {
            StrOrVec::One(s) => vec![s.clone()],
            StrOrVec::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HttpCheck {
    internal_port: u16,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    expected: Option<u16>,
    #[serde(default)]
    expected_output: Option<StrOrVec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestCase {
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    expected_output: Option<StrOrVec>,
    #[serde(default)]
    envs: BTreeMap<String, String>,
    #[serde(default)]
    config_file: Option<String>,
    #[serde(default)]
    should_fail: bool,
    #[serde(default)]
    http_check: Option<HttpCheck>,
    #[serde(default)]
    stderr_allowed: bool,
    #[serde(default)]
    skip_arch: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub example: String,
    pub case: usize,
    pub status: String,
    pub build_s: f64,
    pub detail: String,
}

pub struct E2eConfig {
    pub examples: PathBuf,
    pub filter: Vec<String>,
    pub jobs: usize,
    pub acro_bin: PathBuf,
    pub registry: String,
    pub out: PathBuf,
    pub home: PathBuf,
    pub isolated: bool,
    pub only: Option<std::collections::BTreeSet<(String, usize)>>,
}

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker").args(args).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ensure_registry(registry: &str, home: &Path) -> Result<()> {
    let port = registry.rsplit(':').next().unwrap_or("5001");
    let name = format!("acro-e2e-registry-{port}");
    let _ = docker(&["rm", "-f", &name]);
    let data = home.join(format!("e2e-registry-{port}"));
    let _ = fs::remove_dir_all(&data);
    fs::create_dir_all(&data)?;
    let data = fs::canonicalize(&data)?;
    let mount = format!("{}:/var/lib/registry", data.display());
    docker(&["run", "-d", "--name", &name, "-p", &format!("127.0.0.1:{port}:5000"), "-v", &mount, "registry:3"])?;
    std::thread::sleep(Duration::from_millis(800));
    Ok(())
}

fn ensure_compose_images(file: &Path) {
    let text = fs::read_to_string(file).unwrap_or_default();
    for line in text.lines() {
        let Some(image) = line.trim().strip_prefix("image:") else { continue };
        let image = image.trim().trim_matches(['"', '\'']);
        if image.is_empty() || docker(&["image", "inspect", image]).is_ok() {
            continue;
        }
        let mirrored = if image.split('/').next().is_some_and(|h| h.contains('.') || h.contains(':')) {
            continue;
        } else if image.contains('/') {
            format!("mirror.gcr.io/{image}")
        } else {
            format!("mirror.gcr.io/library/{image}")
        };
        if docker(&["pull", "-q", &mirrored]).is_ok() {
            let _ = docker(&["tag", &mirrored, image]);
            let _ = docker(&["rmi", &mirrored]);
        }
    }
}

fn compose_up(dir: &Path, project: &str) -> Result<Option<String>> {
    let file = dir.join("docker-compose.yml");
    match fs::metadata(&file) {
        Ok(m) if m.len() > 0 => {}
        _ => return Ok(None),
    }
    ensure_compose_images(&file);
    let out = Command::new("docker")
        .args(["compose", "-f"])
        .arg(&file)
        .args(["--project-name", project, "up", "-d", "--wait"])
        .output()?;
    if !out.status.success() {
        bail!("compose up failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(Some(format!("{project}_default")))
}

fn compose_down(dir: &Path, project: &str) {
    let _ = Command::new("docker")
        .args(["compose", "-f"])
        .arg(dir.join("docker-compose.yml"))
        .args(["--project-name", project, "down", "--volumes"])
        .output();
}

fn run_output_check(image: &str, case: &TestCase, network: Option<&str>, name: &str) -> Result<()> {
    let expected = case.expected_output.as_ref().map(|e| e.list()).unwrap_or_default();
    let mut args: Vec<String> = vec!["run".into(), "--rm".into(), "--name".into(), name.into()];
    if let Some(p) = &case.platform {
        args.push("--platform".into());
        args.push(p.clone());
    }
    if let Some(n) = network {
        args.push("--network".into());
        args.push(n.into());
    }
    for (k, v) in &case.envs {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(image.into());
    let mut child = Command::new("docker").args(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let err_buf = Arc::new(Mutex::new(String::new()));
    let eb = err_buf.clone();
    let err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut b = eb.lock().unwrap();
            b.push_str(&line);
            b.push('\n');
        }
    });
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let expected2 = expected.clone();
    std::thread::spawn(move || {
        let mut found = vec![false; expected2.len()];
        let mut all = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            all.push_str(&line);
            all.push('\n');
            for (i, e) in expected2.iter().enumerate() {
                if !found[i] && line.contains(e.as_str()) {
                    found[i] = true;
                }
            }
            if !found.contains(&false) {
                let _ = tx.send(Ok(all.clone()));
                found.push(false);
            }
        }
        let ok = expected2.iter().all(|e| all.contains(e.as_str()));
        let _ = tx.send(if ok { Ok(all) } else { Err(all) });
    });
    let res = rx.recv_timeout(Duration::from_secs(180));
    let stderr_at_match = err_buf.lock().unwrap().clone();
    let _ = docker(&["rm", "-f", name]);
    let _ = child.wait();
    let _ = err_thread.join();
    let stderr_text = err_buf.lock().unwrap().clone();
    match res {
        Ok(Ok(_)) => {
            if !case.stderr_allowed && !stderr_at_match.trim().is_empty() {
                bail!("expected empty stderr, got: {}", stderr_at_match.chars().take(600).collect::<String>());
            }
            Ok(())
        }
        Ok(Err(out)) => bail!(
            "output did not contain {:?}\nstdout: {}\nstderr: {}",
            expected,
            out.chars().take(800).collect::<String>(),
            stderr_text.chars().take(800).collect::<String>()
        ),
        Err(_) => bail!("container timed out"),
    }
}

fn run_http_check(image: &str, case: &TestCase, hc: &HttpCheck, network: Option<&str>, name: &str) -> Result<()> {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        l.local_addr()?.port()
    };
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.into(),
        "-p".into(),
        format!("127.0.0.1:{port}:{}", hc.internal_port),
    ];
    if let Some(n) = network {
        args.push("--network".into());
        args.push(n.into());
    }
    for (k, v) in &case.envs {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(image.into());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    docker(&refs)?;
    std::thread::sleep(Duration::from_secs(2));
    let path = hc.path.clone().unwrap_or_else(|| "/".into());
    let want = hc.expected.unwrap_or(200);
    let url = format!("http://127.0.0.1:{port}{path}");
    let deadline = Instant::now() + Duration::from_secs(35);
    let mut last;
    let result = loop {
        let out = Command::new("curl").args(["-s", "-m", "3", "-w", "\n%{http_code}", &url]).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
        if code.trim().parse::<u16>().ok() == Some(want) {
            let missing: Vec<String> = hc
                .expected_output
                .as_ref()
                .map(|e| e.list())
                .unwrap_or_default()
                .into_iter()
                .filter(|e| !body.contains(e.as_str()))
                .collect();
            if missing.is_empty() {
                break Ok(());
            }
            break Err(anyhow::anyhow!("body missing {:?}: {}", missing, body.chars().take(400).collect::<String>()));
        }
        last = format!("status {} body {}", code.trim(), body.chars().take(200).collect::<String>());
        if Instant::now() > deadline {
            let logs = Command::new("docker").args(["logs", name]).output().ok();
            let logs = logs
                .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
                .unwrap_or_default();
            break Err(anyhow::anyhow!("http check {url} wanted {want}, last {last}\nlogs: {}", logs.chars().rev().take(1500).collect::<String>().chars().rev().collect::<String>()));
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let _ = docker(&["rm", "-f", name]);
    result
}

fn run_case(cfg: &E2eConfig, example: &str, idx: usize, case: &TestCase) -> CaseResult {
    let dir = cfg.examples.join(example);
    let mut r = CaseResult { example: example.into(), case: idx, status: "fail".into(), build_s: 0.0, detail: String::new() };
    let arch = if std::env::consts::ARCH == "x86_64" { "amd64" } else { "arm64" };
    if case.skip_arch.iter().any(|a| a == arch) {
        r.status = "skip".into();
        r.detail = format!("skipArch {arch}");
        return r;
    }
    if let Some(p) = &case.platform
        && !p.ends_with(arch)
    {
        r.status = "skip".into();
        r.detail = format!("needs platform {p}");
        return r;
    }
    let tag = format!("{}/e2e/{}:case{}", cfg.registry, example.to_ascii_lowercase(), idx);
    let home = if cfg.isolated { cfg.home.join(format!("{example}-{idx}")) } else { cfg.home.clone() };
    if cfg.isolated {
        let _ = fs::remove_dir_all(&home);
    }
    let log_dir = cfg.out.with_extension("logs");
    let _ = fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{example}-{idx}.log"));
    let rootfs = std::env::temp_dir().join(format!("acro-e2e-rootfs-{example}-{idx}-{}", std::process::id()));
    let mut cmd = Command::new(&cfg.acro_bin);
    cmd.env("ACRO_ROOTFS", &rootfs);

    cmd.arg("--home").arg(&home).arg("build").arg(&dir).arg("-t").arg(&tag);
    for (k, v) in &case.envs {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    if std::env::var_os("ACRO_E2E_CACHE").is_none() {
        cmd.arg("-e").arg("ACRO_NO_CACHE=1");
    }
    if let Some(c) = &case.config_file {
        cmd.arg("--config").arg(c);
    }
    let start = Instant::now();
    let out = match cmd.stdin(Stdio::null()).output() {
        Ok(o) => o,
        Err(e) => {
            r.detail = format!("spawn: {e}");
            return r;
        }
    };
    r.build_s = start.elapsed().as_secs_f64();
    let _ = fs::write(&log_path, [out.stdout.as_slice(), out.stderr.as_slice()].concat());
    let _ = fs::remove_dir_all(&rootfs);
    if cfg.isolated {
        let _ = fs::remove_dir_all(&home);
    }
    if case.should_fail {
        if out.status.success() {
            r.detail = "expected build failure but build succeeded".into();
        } else {
            r.status = "pass".into();
        }
        return r;
    }
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        r.detail = err.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return r;
    }
    let project = format!("acro-e2e-{}-{}-{}", example.to_ascii_lowercase(), idx, std::process::id());
    let network = match compose_up(&dir, &project) {
        Ok(n) => n,
        Err(e) => {
            r.detail = format!("{e:#}");
            return r;
        }
    };
    let name = format!("acro-e2e-run-{}-{}-{}", example.to_ascii_lowercase(), idx, std::process::id());
    let pulled = docker(&["pull", "-q", &tag]);
    let res = match pulled {
        Err(e) => Err(e),
        Ok(_) => match &case.http_check {
            Some(hc) => run_http_check(&tag, case, hc, network.as_deref(), &name),
            None => run_output_check(&tag, case, network.as_deref(), &name),
        },
    };
    if network.is_some() {
        compose_down(&dir, &project);
    }
    let _ = docker(&["rmi", "-f", &tag]);
    match res {
        Ok(()) => r.status = "pass".into(),
        Err(e) => r.detail = format!("{e:#}"),
    }
    r
}

const MIN_FREE_BYTES: u64 = 5 << 30;

fn free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

fn low_disk(cfg: &E2eConfig) -> bool {
    free_bytes(&cfg.home).map(|f| f < MIN_FREE_BYTES).unwrap_or(false)
}

const MAX_REGISTRY_BYTES: u64 = 2 << 30;

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
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

fn registry_dir(cfg: &E2eConfig) -> PathBuf {
    let port = cfg.registry.rsplit(':').next().unwrap_or("5001");
    cfg.home.join(format!("e2e-registry-{port}"))
}

fn registry_full(cfg: &E2eConfig) -> bool {
    dir_size(&registry_dir(cfg)) > MAX_REGISTRY_BYTES
}

fn infra_failure(detail: &str) -> bool {
    ["/var/lib/registry", "No space left on device", "upload side closed", "operation timed out", "Connection refused", "connection reset"]
        .iter()
        .any(|m| detail.contains(m))
        || detail.contains("signal: 9")
        || detail.contains("exit status: 137")
}

fn reclaim_disk(cfg: &E2eConfig) {
    let before = free_bytes(&cfg.home).unwrap_or(0);
    if let Err(e) = ensure_registry(&cfg.registry, &cfg.home) {
        eprintln!("[e2e] registry restart failed: {e:#}");
    }
    for sub in ["work", "cache", "store", "toolchains"] {
        if !low_disk(cfg) {
            break;
        }
        let _ = fs::remove_dir_all(cfg.home.join(sub));
    }
    let after = free_bytes(&cfg.home).unwrap_or(0);
    eprintln!("[e2e] low disk: reclaimed {:.1} GB ({:.1} GB free)", (after.saturating_sub(before)) as f64 / 1e9, after as f64 / 1e9);
}

fn remove_stale_rootfs() {
    let Ok(rd) = fs::read_dir(std::env::temp_dir()) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("acro-e2e-rootfs-") {
            continue;
        }
        let pid = name.rsplit('-').next().unwrap_or("");
        if !pid.is_empty() && !Path::new("/proc").join(pid).exists() {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

pub fn failed_cases(path: &Path) -> Result<std::collections::BTreeSet<(String, usize)>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut last: BTreeMap<(String, usize), String> = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)?;
        let example = v["example"].as_str().unwrap_or_default().to_string();
        let case = v["case"].as_u64().unwrap_or(0) as usize;
        last.insert((example, case), v["status"].as_str().unwrap_or_default().to_string());
    }
    Ok(last.into_iter().filter(|(_, s)| s == "fail").map(|(k, _)| k).collect())
}

pub fn run(cfg: E2eConfig) -> Result<Vec<CaseResult>> {
    remove_stale_rootfs();
    fs::create_dir_all(&cfg.home)?;
    ensure_registry(&cfg.registry, &cfg.home)?;
    let mut cases: Vec<(String, usize, TestCase)> = Vec::new();
    let mut names: Vec<String> = fs::read_dir(&cfg.examples)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("test.json").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        if !cfg.filter.is_empty() && !cfg.filter.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        let text = fs::read_to_string(cfg.examples.join(&name).join("test.json"))?;
        if text.trim().is_empty() {
            continue;
        }
        let parsed: Vec<TestCase> = json5::from_str(&text).with_context(|| format!("parsing {name}/test.json"))?;
        for (i, c) in parsed.into_iter().enumerate() {
            if cfg.only.as_ref().is_some_and(|o| !o.contains(&(name.clone(), i))) {
                continue;
            }
            cases.push((name.clone(), i, c));
        }
    }
    let queue = Arc::new(Mutex::new(cases.into_iter().map(|(n, i, c)| (n, i, c, false)).collect::<std::collections::VecDeque<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let cfg = Arc::new(cfg);
    let out_file = Arc::new(Mutex::new(fs::OpenOptions::new().create(true).append(true).open(&cfg.out)?));
    let gate = Arc::new(RwLock::new(()));
    let mut handles = Vec::new();
    for _ in 0..cfg.jobs.max(1) {
        let gate = gate.clone();
        let queue = queue.clone();
        let results = results.clone();
        let cfg = cfg.clone();
        let out_file = out_file.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = queue.lock().unwrap().pop_front();
                let Some((name, idx, case, retried)) = next else { break };
                if low_disk(&cfg) || registry_full(&cfg) || !registry_dir(&cfg).exists() {
                    let _w = gate.write().unwrap();
                    if low_disk(&cfg) {
                        reclaim_disk(&cfg);
                    } else if !registry_dir(&cfg).exists() {
                        eprintln!("[e2e] registry data vanished (external cleanup?): recreating");
                        if let Err(e) = ensure_registry(&cfg.registry, &cfg.home) {
                            eprintln!("[e2e] registry restart failed: {e:#}");
                        }
                    } else if registry_full(&cfg) {
                        match ensure_registry(&cfg.registry, &cfg.home) {
                            Ok(()) => eprintln!("[e2e] registry over {} GB: wiped", MAX_REGISTRY_BYTES >> 30),
                            Err(e) => eprintln!("[e2e] registry restart failed: {e:#}"),
                        }
                    }
                }
                let r = {
                    let _r = gate.read().unwrap();
                    run_case(&cfg, &name, idx, &case)
                };
                if r.status == "fail" && !retried && infra_failure(&r.detail) {
                    eprintln!("[e2e] retry  {}/case-{} after infra failure: {}", r.example, r.case, r.detail.lines().last().unwrap_or("").chars().take(160).collect::<String>());
                    queue.lock().unwrap().push_back((name, idx, case, true));
                    continue;
                }
                eprintln!(
                    "[e2e] {:<6} {}/case-{} ({:.1}s) {}",
                    r.status,
                    r.example,
                    r.case,
                    r.build_s,
                    r.detail.lines().next().unwrap_or("")
                );
                {
                    use std::io::Write;
                    let mut f = out_file.lock().unwrap();
                    let _ = writeln!(f, "{}", serde_json::to_string(&r).unwrap_or_default());
                }
                results.lock().unwrap().push(r);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut v = results.lock().unwrap().clone();
    v.sort_by(|a, b| (a.example.clone(), a.case).cmp(&(b.example.clone(), b.case)));
    Ok(v)
}

pub fn summary(results: &[CaseResult]) -> String {
    let pass = results.iter().filter(|r| r.status == "pass").count();
    let fail = results.iter().filter(|r| r.status == "fail").count();
    let skip = results.iter().filter(|r| r.status == "skip").count();
    let mut s = format!("e2e: {pass} passed, {fail} failed, {skip} skipped of {} cases\n", results.len());
    for r in results.iter().filter(|r| r.status == "fail") {
        s.push_str(&format!("  FAIL {}/case-{}: {}\n", r.example, r.case, r.detail.lines().next().unwrap_or("")));
    }
    s
}
