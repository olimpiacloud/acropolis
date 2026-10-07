use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Deserialize)]
pub struct AppSpec {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub expect: String,
}

#[derive(Clone, Debug)]
pub struct BenchConfig {
    pub repo: PathBuf,
    pub apps: Vec<String>,
    pub tools: Vec<String>,
    pub runs: usize,
    pub cpus: String,
    pub memory: Option<String>,
    pub mirror: bool,
    pub out: PathBuf,
    pub acro_bin: PathBuf,
    pub railpack_bin: PathBuf,
    pub railpack_frontend: String,
    pub compression: String,
    pub drop_caches: bool,
    pub scenario: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RunResult {
    pub app: String,
    pub tool: String,
    pub run: usize,
    pub cpus: String,
    pub ok: bool,
    pub verified: bool,
    pub wall_s: f64,
    pub cpu_s: f64,
    pub peak_mem_mb: f64,
    pub net_rx_mb: f64,
    pub disk_write_mb: f64,
    pub image_mb: f64,
    #[serde(default)]
    pub image_pull_s: f64,
    pub digest: String,
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub scenario: String,
}

const REGISTRY_NAME: &str = "acro-bench-registry";
const REGISTRY_PORT: u16 = 5555;
const BUILDER: &str = "acro-bench";

fn sh(cmd: &mut Command) -> Result<String> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{:?} failed: {}\n{}",
            cmd,
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().rev().take(3000).collect::<String>().chars().rev().collect::<String>()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn docker(args: &[&str]) -> Result<String> {
    sh(Command::new("docker").args(args))
}

fn default_iface() -> String {
    fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|t| {
            t.lines().skip(1).find_map(|l| {
                let p: Vec<&str> = l.split_whitespace().collect();
                if p.len() > 1 && p[1] == "00000000" { Some(p[0].to_string()) } else { None }
            })
        })
        .unwrap_or_else(|| "eth0".into())
}

fn rx_bytes(iface: &str) -> u64 {
    fs::read_to_string(format!("/sys/class/net/{iface}/statistics/rx_bytes"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

struct CgStats {
    cpu_usec: u64,
    peak: u64,
    wbytes: u64,
}

fn cg_stats(dir: &Path) -> CgStats {
    let cpu_usec = fs::read_to_string(dir.join("cpu.stat"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.trim().parse().ok())))
        .unwrap_or(0);
    let peak = fs::read_to_string(dir.join("memory.peak")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let wbytes = fs::read_to_string(dir.join("io.stat"))
        .ok()
        .map(|t| {
            t.lines()
                .flat_map(|l| l.split_whitespace())
                .filter_map(|kv| kv.strip_prefix("wbytes=").and_then(|v| v.parse::<u64>().ok()))
                .sum()
        })
        .unwrap_or(0);
    CgStats { cpu_usec, peak, wbytes }
}

fn children_cpu() -> f64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut ru);
        ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 / 1e6 + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 / 1e6
    }
}

fn drop_caches() {
    let _ = Command::new("sync").status();
    let _ = fs::write("/proc/sys/vm/drop_caches", "3");
}

fn reset_registry() -> Result<()> {
    let _ = docker(&["rm", "-f", REGISTRY_NAME]);
    docker(&[
        "run",
        "-d",
        "--name",
        REGISTRY_NAME,
        "-p",
        &format!("127.0.0.1:{REGISTRY_PORT}:5000"),
        "--tmpfs",
        "/var/lib/registry:size=8g",
        "registry:3",
    ])?;
    for _ in 0..50 {
        if Command::new("curl")
            .args(["-sf", &format!("http://127.0.0.1:{REGISTRY_PORT}/v2/")])
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("registry did not start")
}

fn reset_builder(cfg: &BenchConfig) -> Result<PathBuf> {
    let _ = docker(&["buildx", "rm", "-f", BUILDER]);
    let config = if cfg.mirror { cfg.repo.join("bench/buildkitd.toml") } else { cfg.repo.join("bench/buildkitd-nomirror.toml") };
    if !cfg.mirror && !config.exists() {
        fs::write(&config, "[registry.\"localhost:5555\"]\n  http = true\n  insecure = true\n")?;
    }
    let mut args: Vec<String> = vec![
        "buildx".into(),
        "create".into(),
        "--name".into(),
        BUILDER.into(),
        "--driver".into(),
        "docker-container".into(),
        "--driver-opt".into(),
        "network=host".into(),
        "--driver-opt".into(),
        format!("cpuset-cpus={}", cfg.cpus),
        "--buildkitd-config".into(),
        config.to_string_lossy().into_owned(),
        "--bootstrap".into(),
    ];
    if let Some(m) = &cfg.memory {
        args.push("--driver-opt".into());
        args.push(format!("memory={m}"));
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    docker(&refs)?;
    builder_cgroup()
}

fn builder_cgroup() -> Result<PathBuf> {
    let id = docker(&["inspect", "-f", "{{.Id}}", &format!("buildx_buildkit_{BUILDER}0")])?;
    let id = id.trim();
    let candidates = [
        PathBuf::from(format!("/sys/fs/cgroup/system.slice/docker-{id}.scope")),
        PathBuf::from(format!("/sys/fs/cgroup/docker/{id}")),
    ];
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
    }
    bail!("cannot find cgroup of buildkit container {id}")
}

fn image_size(reference: &str) -> Result<(f64, String)> {
    let (repo, tag) = reference
        .trim_start_matches(&format!("localhost:{REGISTRY_PORT}/"))
        .rsplit_once(':')
        .context("bad reference")?;
    let accept = "application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json,application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json";
    let url = format!("http://127.0.0.1:{REGISTRY_PORT}/v2/{repo}/manifests/{tag}");
    let body = sh(Command::new("curl").args(["-sf", "-H", &format!("Accept: {accept}"), &url]))?;
    let mut v: serde_json::Value = serde_json::from_str(&body)?;
    let digest = sh(Command::new("curl").args(["-sfI", "-H", &format!("Accept: {accept}"), &url]))?
        .lines()
        .find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("docker-content-digest:").map(|d| d.trim().to_string())
        })
        .unwrap_or_default();
    if v.get("manifests").is_some() {
        let m = v["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["platform"]["architecture"] == "amd64")
            .cloned()
            .context("no amd64 manifest")?;
        let d = m["digest"].as_str().unwrap_or("");
        let url = format!("http://127.0.0.1:{REGISTRY_PORT}/v2/{repo}/manifests/{d}");
        v = serde_json::from_str(&sh(Command::new("curl").args(["-sf", "-H", &format!("Accept: {accept}"), &url]))?)?;
    }
    let size: u64 = v["layers"].as_array().map(|l| l.iter().filter_map(|x| x["size"].as_u64()).sum()).unwrap_or(0)
        + v["config"]["size"].as_u64().unwrap_or(0);
    Ok((size as f64 / 1e6, digest))
}

fn verify(app: &AppSpec, reference: &str) -> Result<()> {
    let name = format!("acro-bench-verify-{}", std::process::id());
    let _ = docker(&["rm", "-f", &name]);
    docker(&["pull", "-q", reference])?;
    let host_port = 39000 + (std::process::id() % 1000) as u16;
    docker(&["run", "-d", "--name", &name, "-p", &format!("127.0.0.1:{host_port}:{}", app.port), reference])?;
    let url = format!("http://127.0.0.1:{host_port}{}", app.path);
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut last = String::new();
    let result = loop {
        let out = Command::new("curl").args(["-s", "-m", "3", &url]).output();
        if let Ok(o) = out {
            let body = String::from_utf8_lossy(&o.stdout).into_owned();
            if body.contains(&app.expect) {
                break Ok(());
            }
            last = body;
        }
        if Instant::now() > deadline {
            let logs = docker(&["logs", &name]).unwrap_or_default();
            break Err(anyhow::anyhow!(
                "response from {url} did not contain {:?}: {}\nlogs: {}",
                app.expect,
                last.chars().take(300).collect::<String>(),
                logs.chars().take(1000).collect::<String>()
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
    };
    let _ = docker(&["rm", "-f", &name]);
    let _ = docker(&["rmi", "-f", reference]);
    result
}

fn acro_variant(cfg: &BenchConfig, tool: &str) -> Option<(String, PathBuf)> {
    if tool == "acro" {
        return Some(("acro".into(), cfg.acro_bin.clone()));
    }
    let (label, path) = tool.strip_prefix("acro@")?.split_once('=')?;
    Some((format!("acro@{label}"), PathBuf::from(path)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Cold,
    Prime,
    Rebuild,
}

const TOUCH_CANDIDATES: &[&str] =
    &["src/app/page.tsx", "src/routes/index.tsx", "src/App.tsx", "src/main.rs", "main.go", "index.js", "server.js", "app.py", "main.py"];

fn touch_source(dir: &Path) -> Result<()> {
    let f = TOUCH_CANDIDATES.iter().map(|c| dir.join(c)).find(|p| p.exists()).ok_or_else(|| anyhow!("no source file to modify in {}", dir.display()))?;
    let mut text = fs::read_to_string(&f)?;
    text.push_str(&format!("\n// rebuild {}\n", chrono_now()));
    fs::write(&f, text)?;
    Ok(())
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let _ = fs::remove_dir_all(dst);
    let ok = Command::new("cp").arg("-a").arg(src).arg(dst).status()?.success();
    if !ok {
        bail!("copying {} failed", src.display());
    }
    Ok(())
}

fn run_one(cfg: &BenchConfig, app: &AppSpec, tool_spec: &str, run: usize, iface: &str, log_dir: &Path) -> RunResult {
    let dir = cfg.repo.join("bench/apps").join(&app.name);
    run_phase(cfg, app, tool_spec, run, iface, log_dir, Phase::Cold, &dir)
}

#[allow(clippy::too_many_arguments)]
fn run_phase(cfg: &BenchConfig, app: &AppSpec, tool_spec: &str, run: usize, iface: &str, log_dir: &Path, phase: Phase, app_dir: &Path) -> RunResult {
    let acro = acro_variant(cfg, tool_spec);
    let tool_label = acro.as_ref().map(|(l, _)| l.clone()).unwrap_or_else(|| tool_spec.to_string());
    let tool_slug: String = tool_label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect();
    let tool = if acro.is_some() { "acro" } else { tool_spec };
    let mut r = RunResult {
        app: app.name.clone(),
        tool: tool_label.clone(),
        run,
        cpus: cfg.cpus.clone(),
        started_at: chrono_now(),
        ..Default::default()
    };
    let app_dir = app_dir.to_path_buf();
    r.scenario = match phase {
        Phase::Cold => "cold",
        Phase::Prime => "prime",
        Phase::Rebuild => "rebuild",
    }
    .to_string();
    let reference = format!("localhost:{REGISTRY_PORT}/bench/{}:{tool_slug}-{run}", app.name);
    let suffix = if phase == Phase::Cold { String::new() } else { format!("-{}", r.scenario) };
    let log_path = log_dir.join(format!("{}-{tool_slug}-{run}{suffix}.log", app.name));
    let res = (|| -> Result<()> {
        if phase != Phase::Rebuild {
            reset_registry()?;
        }
        let mut builder_cg = None;
        let acro_home = std::env::temp_dir().join(format!("acro-bench-home-{}", std::process::id()));
        if phase != Phase::Rebuild {
            let _ = fs::remove_dir_all(&acro_home);
        }
        let cg = PathBuf::from(format!("/sys/fs/cgroup/acro-bench-{}", std::process::id()));
        if tool == "acro" {
            let _ = fs::remove_dir(&cg);
            fs::create_dir_all(&cg)?;
            fs::write(cg.join("cpuset.cpus"), &cfg.cpus)?;
            if let Some(m) = &cfg.memory {
                fs::write(cg.join("memory.max"), parse_mem(m).to_string())?;
            }
        } else {
            builder_cg = Some(if phase == Phase::Rebuild { builder_cgroup()? } else { reset_builder(cfg)? });
        }
        if cfg.drop_caches && phase != Phase::Rebuild {
            drop_caches();
        }
        let before_cg = builder_cg.as_ref().map(|p| cg_stats(p));
        let rx0 = rx_bytes(iface);
        let cpu0 = children_cpu();
        let log = fs::File::create(&log_path)?;
        let start = Instant::now();
        let status = match tool {
            "docker" => {
                let dockerfile = cfg.repo.join("bench/dockerfiles").join(format!("{}.Dockerfile", app.name));
                Command::new("docker")
                    .args(["buildx", "build", "--builder", BUILDER, "--progress", "plain", "--push", "-t", &reference, "-f"])
                    .arg(&dockerfile)
                    .arg(&app_dir)
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .status()?
            }
            "railpack" => {
                let plan = std::env::temp_dir().join(format!("railpack-plan-{}.json", std::process::id()));
                let st = Command::new(&cfg.railpack_bin)
                    .args(["prepare"])
                    .arg(&app_dir)
                    .arg("--plan-out")
                    .arg(&plan)
                    .stdout(log.try_clone()?)
                    .stderr(log.try_clone()?)
                    .status()?;
                if !st.success() {
                    st
                } else {
                    Command::new("docker")
                        .args(["buildx", "build", "--builder", BUILDER, "--progress", "plain", "--push", "-t", &reference])
                        .arg("--build-arg")
                        .arg(format!("BUILDKIT_SYNTAX={}", cfg.railpack_frontend))
                        .arg("-f")
                        .arg(&plan)
                        .arg(&app_dir)
                        .stdout(log.try_clone()?)
                        .stderr(log)
                        .status()?
                }
            }
            "acro" => {
                let mut c = Command::new("/bin/sh");
                c.arg("-c")
                    .arg(format!("echo $$ > {}/cgroup.procs && exec \"$@\"", cg.display()))
                    .arg("sh")
                    .arg(acro.as_ref().map(|(_, b)| b.clone()).unwrap_or_else(|| cfg.acro_bin.clone()))
                    .arg("--home")
                    .arg(&acro_home)
                    .arg("--events")
                    .arg("json");
                if cfg.mirror {
                    c.arg("--mirror").arg("docker.io=mirror.gcr.io");
                }
                c.arg("build").arg(&app_dir).arg("-t").arg(&reference).arg("--compression").arg(&cfg.compression);
                c.stdout(log.try_clone()?).stderr(log).status()?
            }
            other => bail!("unknown tool {other}"),
        };
        let wall = start.elapsed().as_secs_f64();
        let _ = Command::new("sync").status();
        let rx1 = rx_bytes(iface);
        let cpu1 = children_cpu();
        r.wall_s = wall;
        r.net_rx_mb = (rx1.saturating_sub(rx0)) as f64 / 1e6;
        let client_cpu = cpu1 - cpu0;
        if tool == "acro" {
            let s = cg_stats(&cg);
            r.cpu_s = s.cpu_usec as f64 / 1e6;
            r.peak_mem_mb = s.peak as f64 / 1e6;
            r.disk_write_mb = s.wbytes as f64 / 1e6;
            if phase != Phase::Prime {
                let _ = fs::remove_dir_all(&acro_home);
            }
            let _ = fs::remove_dir(&cg);
        } else if let (Some(p), Some(b)) = (builder_cg.as_ref(), before_cg) {
            let s = cg_stats(p);
            r.cpu_s = (s.cpu_usec - b.cpu_usec) as f64 / 1e6 + client_cpu;
            r.peak_mem_mb = s.peak as f64 / 1e6;
            r.disk_write_mb = (s.wbytes - b.wbytes) as f64 / 1e6;
        }
        if !status.success() {
            let tail = fs::read_to_string(&log_path).unwrap_or_default();
            let tail: String = tail.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            bail!("build failed ({status}):\n{tail}");
        }
        r.ok = true;
        r.image_pull_s = image_pull_seconds(&fs::read_to_string(&log_path).unwrap_or_default(), tool);
        let (size, digest) = image_size(&reference)?;
        r.image_mb = size;
        r.digest = digest;
        verify(app, &reference)?;
        r.verified = true;
        Ok(())
    })();
    if let Err(e) = res {
        r.error = Some(format!("{e:#}"));
    }
    r
}

pub fn image_pull_seconds(log: &str, tool: &str) -> f64 {
    if tool.starts_with("acro") {
        let mut best = 0.0f64;
        for line in log.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && v["type"] == "step_finished"
                && (v["id"] == "base" || v["id"] == "copy-base")
            {
                best += v["ms"].as_f64().unwrap_or(0.0) / 1000.0;
            }
        }
        return best;
    }
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut done: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for line in log.lines() {
        let Some(rest) = line.strip_prefix('#') else { continue };
        let Some((id, text)) = rest.split_once(' ') else { continue };
        if text.starts_with("docker-image://") || text.contains("] FROM ") {
            names.insert(id.to_string(), text.to_string());
        } else if let Some(t) = text.strip_prefix("DONE ").and_then(|t| t.trim_end_matches('s').parse::<f64>().ok()) {
            let e = done.entry(id.to_string()).or_insert(0.0);
            if t > *e {
                *e = t;
            }
        }
    }
    names.iter().filter_map(|(id, _)| done.get(id)).cloned().fold(0.0, f64::max)
}

fn parse_mem(s: &str) -> u64 {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix('g') {
        (n, 1u64 << 30)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1 << 20)
    } else {
        (s.as_str(), 1)
    };
    num.parse::<u64>().unwrap_or(0) * mult
}

fn chrono_now() -> String {
    let out = Command::new("date").arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ").output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

pub fn run(cfg: BenchConfig) -> Result<Vec<RunResult>> {
    let specs: Vec<AppSpec> = serde_json::from_slice(&fs::read(cfg.repo.join("bench/apps.json"))?)?;
    let iface = default_iface();
    fs::create_dir_all(cfg.out.parent().unwrap_or(Path::new(".")))?;
    let log_dir = cfg.out.with_extension("logs");
    fs::create_dir_all(&log_dir)?;
    let mut results = Vec::new();
    let mut out = fs::OpenOptions::new().create(true).append(true).open(&cfg.out)?;
    for run in 1..=cfg.runs {
        for name in &cfg.apps {
            let app = specs.iter().find(|s| &s.name == name).with_context(|| format!("unknown app {name}"))?;
            for tool in &cfg.tools {
                eprintln!("[bench] {} {} run {} on cpus {}", app.name, tool, run, cfg.cpus);
                let r = if cfg.scenario == "rebuild" {
                    let work = std::env::temp_dir().join(format!("acro-bench-app-{}", std::process::id())).join(&app.name);
                    let prepared = fs::create_dir_all(work.parent().unwrap())
                        .map_err(anyhow::Error::from)
                        .and_then(|_| copy_dir(&cfg.repo.join("bench/apps").join(&app.name), &work));
                    let prime = match prepared {
                        Ok(()) => run_phase(&cfg, app, tool, run, &iface, &log_dir, Phase::Prime, &work),
                        Err(e) => RunResult { app: app.name.clone(), tool: tool.clone(), run, error: Some(format!("{e:#}")), ..Default::default() },
                    };
                    eprintln!("[bench]   prime ok={} wall={:.1}s", prime.ok, prime.wall_s);
                    if prime.ok {
                        match touch_source(&work) {
                            Ok(()) => run_phase(&cfg, app, tool, run, &iface, &log_dir, Phase::Rebuild, &work),
                            Err(e) => RunResult { scenario: "rebuild".into(), error: Some(format!("{e:#}")), ..prime },
                        }
                    } else {
                        RunResult { scenario: "rebuild".into(), ok: false, error: Some(format!("prime failed: {}", prime.error.clone().unwrap_or_default())), ..prime }
                    }
                } else {
                    run_one(&cfg, app, tool, run, &iface, &log_dir)
                };
                eprintln!(
                    "[bench]   ok={} verified={} wall={:.1}s cpu={:.1}s mem={:.0}MB net={:.1}MB disk={:.1}MB image={:.1}MB {}",
                    r.ok,
                    r.verified,
                    r.wall_s,
                    r.cpu_s,
                    r.peak_mem_mb,
                    r.net_rx_mb,
                    r.disk_write_mb,
                    r.image_mb,
                    r.error.as_deref().map(|e| e.lines().next().unwrap_or("")).unwrap_or("")
                );
                writeln!(out, "{}", serde_json::to_string(&r)?)?;
                results.push(r);
            }
        }
    }
    let _ = docker(&["buildx", "rm", "-f", BUILDER]);
    let _ = docker(&["rm", "-f", REGISTRY_NAME]);
    Ok(results)
}

pub fn backfill(results: &mut [RunResult], log_dir: &Path) {
    for r in results.iter_mut() {
        if r.image_pull_s == 0.0 {
            let p = log_dir.join(format!("{}-{}-{}.log", r.app, r.tool, r.run));
            if let Ok(text) = fs::read_to_string(p) {
                r.image_pull_s = image_pull_seconds(&text, &r.tool);
            }
        }
    }
}

pub fn summarize(results: &[RunResult]) -> String {
    let mut apps: Vec<String> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for r in results {
        if !apps.contains(&r.app) {
            apps.push(r.app.clone());
        }
        if !tools.contains(&r.tool) {
            tools.push(r.tool.clone());
        }
    }
    let range = |vals: Vec<f64>, prec: usize| -> String {
        if vals.is_empty() {
            return "—".into();
        }
        let lo = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if (hi - lo).abs() < 10f64.powi(-(prec as i32)) {
            format!("{lo:.prec$}")
        } else {
            format!("{lo:.prec$}–{hi:.prec$}")
        }
    };
    let mut s = String::new();
    let metrics: [(&str, fn(&RunResult) -> f64, usize); 7] = [
        ("wall time (s)", |r| r.wall_s, 1),
        ("CPU-seconds", |r| r.cpu_s, 1),
        ("peak memory (MB)", |r| r.peak_mem_mb, 0),
        ("downloaded (MB)", |r| r.net_rx_mb, 0),
        ("disk writes (MB)", |r| r.disk_write_mb, 0),
        ("image size (MB)", |r| r.image_mb, 1),
        ("base/builder image pull (s)", |r| r.image_pull_s, 1),
    ];
    for (title, f, prec) in metrics {
        s.push_str(&format!("\n### {title}\n\n| app | {} |\n|---|{}\n", tools.join(" | "), "---|".repeat(tools.len())));
        for a in &apps {
            let mut row = format!("| {a} |");
            for t in &tools {
                let rs: Vec<&RunResult> = results.iter().filter(|r| &r.app == a && &r.tool == t).collect();
                let good: Vec<f64> = rs.iter().filter(|r| r.ok && r.verified).map(|r| f(r)).collect();
                let failed = rs.len() - good.len();
                let mut cell = range(good, prec);
                if failed > 0 {
                    cell.push_str(&format!(" ({failed} failed)"));
                }
                row.push_str(&format!(" {cell} |"));
            }
            s.push_str(&row);
            s.push('\n');
        }
    }
    s
}
