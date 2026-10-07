use crate::detect::Env;
use crate::ignore::Ignore;
use crate::plan::{Action, LayerFrom, Plan, Step};
use crate::source;
use acro_exec::{Cmd, Executor};
use acro_fetch::Fetcher;
use acro_npm::{InstallOptions, InstallPlan, PackageLock, Tarballs};
use acro_oci::assemble::{self, ConfigPatch};
use acro_oci::image::Platform;
use acro_oci::layer::{self, Layer, LayerOptions};
use acro_oci::tar::TarWriter;
use acro_oci::{Reference, Registry, ResolvedImage};
use acro_store::Store;
use acro_toolchain::Installed;
use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone)]
pub struct BuildOptions {
    pub app_dir: PathBuf,
    pub home: PathBuf,
    pub target: Option<Reference>,
    pub env: Env,
    pub layer: LayerOptions,
    pub mirrors: Vec<(String, String)>,
    pub concurrency: usize,
    pub keep_work: bool,
    pub platform: Platform,
    pub oci_out: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub plan_hash: String,
    pub manifest_digest: String,
    pub reference: Option<String>,
    pub layers: Vec<Layer>,
    pub base: Option<String>,
    pub ms: u64,
    pub tools: Vec<(String, String)>,
}

pub struct NpmState {
    pub plan: InstallPlan,
    pub tarballs: Tarballs,
}

#[derive(Clone)]
enum Out {
    None,
    Base(Arc<ResolvedImage>),
    Tool(Installed),
    Npm(Arc<NpmState>),
    Layer(Layer),
    Pushed(String),
    Upper(PathBuf),
}

struct AppCache {
    dir: PathBuf,
    _lock: std::fs::File,
}

fn open_app_cache(opts: &BuildOptions) -> Option<AppCache> {
    if opts.env.flag("NO_CACHE") {
        return None;
    }
    let key = opts.env.config("CACHE_KEY").map(|(v, _)| v).unwrap_or_else(|| {
        let canon = std::fs::canonicalize(&opts.app_dir).unwrap_or_else(|_| opts.app_dir.clone());
        acro_store::sha256_bytes(canon.to_string_lossy().as_bytes()).hex()[..16].to_string()
    });
    let key: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let dir = opts.home.join("cache").join("apps").join(&key);
    std::fs::create_dir_all(&dir).ok()?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock")).ok()?;
    let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        acro_events::log("cache", format!("app cache {key} is in use by another build; building without it"));
        return None;
    }
    crate::gc::touch(&dir.join(".lock"));
    Some(AppCache { dir, _lock: lock })
}

const NODE_CACHE_DIRS: &[&str] = &[".next/cache", "node_modules/.cache"];

fn restore_node_caches(cache: &Path, cwd: &Path) {
    for rel in NODE_CACHE_DIRS {
        let saved = cache.join("node").join(rel.replace('/', "__"));
        let live = cwd.join(rel);
        if saved.is_dir() && !live.exists() {
            if let Some(parent) = live.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&saved, &live);
        }
    }
}

fn save_node_caches(cache: &Path, cwd: &Path) {
    for rel in NODE_CACHE_DIRS {
        let live = cwd.join(rel);
        if live.is_dir() {
            let saved = cache.join("node").join(rel.replace('/', "__"));
            let _ = std::fs::create_dir_all(cache.join("node"));
            let _ = std::fs::remove_dir_all(&saved);
            let _ = std::fs::rename(&live, &saved);
        }
    }
}

fn find_node_modules(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if name == "node_modules" {
                out.push(r);
            } else if depth < 5 && !name.starts_with('.') {
                stack.push((e.path(), r, depth + 1));
            }
        }
    }
    out
}

fn stash_node_modules(cache: &Path, src: &Path) {
    let prev = cache.join("nm-prev");
    if prev.exists() {
        let old = cache.join(format!(".nm-old-{}", std::process::id()));
        if std::fs::rename(&prev, &old).is_ok() {
            std::thread::spawn(move || {
                let _ = std::fs::remove_dir_all(old);
            });
        }
    }
    let Ok(key) = std::fs::read_to_string(cache.join("nm.key")) else { return };
    let _ = std::fs::remove_file(cache.join("nm.key"));
    let dirs = find_node_modules(src);
    if dirs.is_empty() || std::fs::create_dir_all(&prev).is_err() {
        return;
    }
    let mut list = Vec::new();
    for (i, rel) in dirs.iter().enumerate() {
        if std::fs::rename(src.join(rel), prev.join(i.to_string())).is_ok() {
            list.push(format!("{i}\t{rel}"));
        }
    }
    let _ = std::fs::write(prev.join(".list"), list.join("\n"));
    let _ = std::fs::write(prev.join(".key"), key);
}

fn restore_node_modules(cache: &Path, src: &Path, key: &str) -> bool {
    let prev = cache.join("nm-prev");
    if std::fs::read_to_string(prev.join(".key")).ok().as_deref() != Some(key) {
        return false;
    }
    let Ok(list) = std::fs::read_to_string(prev.join(".list")) else { return false };
    for line in list.lines() {
        let Some((i, rel)) = line.split_once('\t') else { continue };
        let dest = src.join(rel);
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if dest.exists() || std::fs::rename(prev.join(i), &dest).is_err() {
            return false;
        }
    }
    let _ = std::fs::remove_dir_all(&prev);
    true
}

struct Ctx {
    cache: Option<AppCache>,
    opts: BuildOptions,
    plan: Plan,
    work: PathBuf,
    src: PathBuf,
    store: Arc<Store>,
    fetcher: Fetcher,
    registry: Arc<Registry>,
    exec: Arc<dyn Executor>,
    out: Mutex<HashMap<String, Out>>,
    base_copy: Mutex<Option<tokio::task::JoinHandle<Result<u64>>>>,
}

type StepFuture = Shared<BoxFuture<'static, Result<(), Arc<anyhow::Error>>>>;

impl Ctx {
    fn cache_path(&self, kind: &str) -> PathBuf {
        match &self.cache {
            Some(c) => c.dir.join(kind),
            None => self.work.join(kind),
        }
    }

    fn get(&self, id: &str) -> Out {
        self.out.lock().unwrap().get(id).cloned().unwrap_or(Out::None)
    }

    fn dep_outputs(&self, step: &Step) -> Vec<Out> {
        step.deps.iter().map(|d| self.get(d)).collect()
    }

    fn base(&self) -> Option<Arc<ResolvedImage>> {
        self.out.lock().unwrap().values().find_map(|o| if let Out::Base(b) = o { Some(b.clone()) } else { None })
    }

    fn tools(&self) -> Vec<Installed> {
        let mut v: Vec<Installed> = self
            .out
            .lock()
            .unwrap()
            .values()
            .filter_map(|o| if let Out::Tool(t) = o { Some(t.clone()) } else { None })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    fn subst(&self, s: &str) -> String {
        let mut out = s
            .replace("{cargo_target}", &self.cache_path("cargo-target").to_string_lossy())
            .replace("{work}", &self.work.to_string_lossy())
            .replace("{src}", &self.src.to_string_lossy());
        let mut from = 0;
        while let Some(i) = out[from..].find("{env:") {
            let start = from + i;
            let Some(end) = out[start..].find('}') else { break };
            let name = out[start + 5..start + end].to_string();
            let value = self.opts.env.vars.get(&name).cloned().unwrap_or_default();
            out.replace_range(start..start + end + 1, &value);
            from = start + value.len();
        }
        while let Some(start) = out.find("{tool:") {
            let Some(end) = out[start..].find('}') else { break };
            let name = out[start + 6..start + end].to_string();
            let root = self.tools().into_iter().find(|t| t.name == name).map(|t| t.root.to_string_lossy().into_owned()).unwrap_or_default();
            out.replace_range(start..start + end + 1, &root);
        }
        out
    }

    fn subst_versions(&self, s: &str) -> String {
        let mut out = s.to_string();
        let mut from = 0;
        while let Some(i) = out[from..].find("{version:") {
            let start = from + i;
            let Some(end) = out[start..].find('}') else { break };
            let inner = out[start + 9..start + end].to_string();
            let (name, fallback) = inner.split_once('|').unwrap_or((inner.as_str(), ""));
            let v = self.tools().into_iter().find(|t| t.name == name).map(|t| t.version).unwrap_or_else(|| fallback.to_string());
            out.replace_range(start..start + end + 1, &v);
            from = start + v.len();
        }
        out
    }
}

pub fn make_registry(client: reqwest::Client, mirrors: &[(String, String)]) -> Registry {
    let mut reg = Registry::new(client);
    for (from, to) in mirrors {
        reg = reg.with_mirror(from, to);
    }
    reg
}

pub async fn execute(plan: Plan, opts: BuildOptions, exec: Arc<dyn Executor>) -> Result<BuildResult> {
    let start = Instant::now();
    let mut opts = opts;
    opts.home = std::path::absolute(&opts.home)?;
    opts.app_dir = std::path::absolute(&opts.app_dir)?;
    let store = Arc::new(Store::open(opts.home.join("store"))?);
    let fetcher = Fetcher::new(store.clone(), opts.concurrency)?;
    let registry = Arc::new(make_registry(fetcher.client().clone(), &opts.mirrors));
    let work = opts.home.join("work").join(format!("{}-{}", &plan.hash[..12], std::process::id()));
    std::fs::create_dir_all(&work)?;

    acro_events::emit(acro_events::Event::PlanReady { hash: plan.hash.clone(), steps: plan.steps.len() });
    let home_lock = crate::gc::shared(&opts.home);
    let cache = open_app_cache(&opts);
    let stable_src = opts.env.config("CACHE_SRC").map(|(v, _)| v != "0").unwrap_or(true);
    let src = match cache.as_ref().filter(|_| stable_src) {
        Some(c) => {
            acro_events::log("cache", format!("using app cache {}", c.dir.display()));
            let src = c.dir.join("src");
            if src.exists() {
                stash_node_modules(&c.dir, &src);
                let old = c.dir.join(format!(".src-old-{}", std::process::id()));
                if std::fs::rename(&src, &old).is_ok() {
                    std::thread::spawn(move || {
                        let _ = std::fs::remove_dir_all(old);
                    });
                } else {
                    std::fs::remove_dir_all(&src)?;
                }
            }
            if let Ok(rd) = std::fs::read_dir(&c.dir) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let mine = name.ends_with(&format!("-{}", std::process::id())) || name.contains(&format!("-{}-", std::process::id()));
                    if (name.starts_with(".src-old-") || name.starts_with(".nm-old-")) && !mine {
                        let p = e.path();
                        std::thread::spawn(move || {
                            let _ = std::fs::remove_dir_all(p);
                        });
                    }
                }
            }
            src
        }
        None => work.join("src"),
    };
    let ctx = Arc::new(Ctx {
        cache,
        opts: opts.clone(),
        plan: plan.clone(),
        work: work.clone(),
        src,
        store,
        fetcher,
        registry,
        exec,
        out: Mutex::new(HashMap::new()),
        base_copy: Mutex::new(None),
    });
    let secs = |name: &str| opts.env.config(name).and_then(|(v, _)| v.parse::<u64>().ok()).filter(|s| *s > 0).map(std::time::Duration::from_secs);
    let step_timeout = secs("STEP_TIMEOUT");
    let build_timeout = secs("BUILD_TIMEOUT");
    let mut futs: HashMap<String, StepFuture> = HashMap::new();
    let mut order = Vec::new();
    for step in &plan.steps {
        let deps: Vec<StepFuture> = step
            .deps
            .iter()
            .map(|d| futs.get(d).cloned().ok_or_else(|| anyhow!("step {} depends on unknown {}", step.id, d)))
            .collect::<Result<_>>()?;
        let ctx2 = ctx.clone();
        let step2 = step.clone();
        let fut = async move {
            for d in deps {
                d.await?;
            }
            let guard = acro_events::step(&step2.id, &step2.name);
            let touch_tool = |out: &Out| {
                if let Out::Tool(t) = out {
                    crate::gc::touch(&t.root.join(".acro-complete"));
                }
            };
            let res = match step_timeout {
                Some(t) => match tokio::time::timeout(t, run_step(&ctx2, &step2)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow!("timed out after {}s (ACRO_STEP_TIMEOUT)", t.as_secs())),
                },
                None => run_step(&ctx2, &step2).await,
            };
            match res {
                Ok(out) => {
                    touch_tool(&out);
                    ctx2.out.lock().unwrap().insert(step2.id.clone(), out);
                    guard.finish();
                    Ok(())
                }
                Err(e) => {
                    guard.fail(&format!("{e:#}"));
                    Err(Arc::new(e.context(format!("step {} ({})", step2.id, step2.name))))
                }
            }
        }
        .boxed()
        .shared();
        let handle = tokio::spawn(fut.clone());
        order.push(handle);
        futs.insert(step.id.clone(), fut);
    }
    let aborts: Vec<tokio::task::AbortHandle> = order.iter().map(|h| h.abort_handle()).collect();
    let mut pending: futures::stream::FuturesUnordered<_> = order.into_iter().collect();
    let mut first_err: Option<Arc<anyhow::Error>> = None;
    let deadline = async {
        match build_timeout {
            Some(t) => tokio::time::sleep(t).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let interrupted: Option<String> = tokio::select! {
            next = futures::StreamExt::next(&mut pending) => match next {
                None => break,
                Some(Ok(Ok(()))) => None,
                Some(Ok(Err(e))) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    None
                }
                Some(Err(e)) if e.is_cancelled() => None,
                Some(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(Arc::new(anyhow!("task panicked: {e}")));
                    }
                    None
                }
            },
            _ = &mut deadline => Some(format!("build timed out after {}s (ACRO_BUILD_TIMEOUT)", build_timeout.map(|t| t.as_secs()).unwrap_or(0))),
            _ = tokio::signal::ctrl_c() => Some("build interrupted (SIGINT)".to_string()),
            _ = sigterm.recv() => Some("build interrupted (SIGTERM)".to_string()),
        };
        if let Some(reason) = interrupted
            && first_err.is_none()
        {
            first_err = Some(Arc::new(anyhow!(reason)));
        }
        if first_err.is_some() {
            for a in &aborts {
                a.abort();
            }
            if let Some(h) = ctx.base_copy.lock().unwrap().take() {
                h.abort();
            }
        }
    }
    if !opts.keep_work {
        let _ = std::fs::remove_dir_all(&work);
    }
    drop(home_lock);
    if let Some(max) = opts.env.config("CACHE_MAX").and_then(|(v, _)| crate::gc::parse_size(&v)) {
        let rootfs = std::env::var_os("ACRO_ROOTFS").map(PathBuf::from).unwrap_or_else(|| opts.home.join("rootfs"));
        let home = opts.home.clone();
        let _ = tokio::task::spawn_blocking(move || crate::gc::collect(&home, &rootfs, max)).await;
    }
    if let Some(e) = first_err {
        return Err(anyhow!("{e:#}"));
    }
    let layers: Vec<Layer> = plan
        .image
        .layers
        .iter()
        .filter_map(|id| if let Out::Layer(l) = ctx.get(id) { Some(l) } else { None })
        .collect();
    let digest = match ctx.get("push") {
        Out::Pushed(d) => d,
        _ => String::new(),
    };
    Ok(BuildResult {
        plan_hash: plan.hash.clone(),
        manifest_digest: digest,
        reference: opts.target.as_ref().map(|t| t.to_string()),
        layers,
        base: ctx.base().map(|b| b.reference.to_string()),
        ms: start.elapsed().as_millis() as u64,
        tools: ctx.tools().into_iter().map(|t| (t.name, t.version)).collect(),
    })
}

async fn run_step(ctx: &Arc<Ctx>, step: &Step) -> Result<Out> {
    match &step.action {
        Action::ResolveBase { image } => {
            let image = resolve_alias(ctx, image).await?;
            resolve_base(ctx, step, &image).await
        }
        Action::ResolveNodeBase { spec, variant } => {
            let version = acro_toolchain::node::resolve(&ctx.fetcher, spec).await?;
            resolve_base(ctx, step, &format!("node:{version}-{variant}")).await
        }
        Action::ResolveBaseLatest { template, github } => {
            let tag = github_latest_tag(github).await?;
            resolve_base(ctx, step, &template.replace("{tag}", &tag)).await
        }
        Action::CopyBase => {
            let handle = ctx.base_copy.lock().unwrap().take();
            let Some(handle) = handle else { return Ok(Out::None) };
            let n = handle.await??;
            acro_events::log(&step.id, format!("copied {:.1} MB", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::Toolchain { tool, spec, parts } => {
            let fetcher = &ctx.fetcher;
            match tool.as_str() {
                "node" => {
                    let fuzzy = acro_semver::fuzzy_version(spec);
                    let version = acro_toolchain::node::resolve(fetcher, &fuzzy).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("node-{version}"));
                    let marker = dest.join(".acro-complete");
                    if marker.exists() {
                        return Ok(Out::Tool(Installed {
                            name: "node".into(),
                            version,
                            bin_dir: dest.join("bin"),
                            root: dest,
                            archive: None,
                        }));
                    }
                    let p = acro_toolchain::node::Parts {
                        npm: parts.iter().any(|p| p == "npm"),
                        corepack: parts.iter().any(|p| p == "corepack"),
                        headers: parts.iter().any(|p| p == "headers"),
                    };
                    let tmp = staging(&dest);
                    acro_toolchain::node::install(fetcher, &version, &tmp, p).await?;
                    publish(&tmp, &dest)?;
                    acro_events::log(&step.id, format!("node {version}"));
                    Ok(Out::Tool(Installed { name: "node".into(), version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "go" => {
                    let version = acro_toolchain::go::resolve(fetcher, spec).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("go-{version}"));
                    let marker = dest.join(".acro-complete");
                    if marker.exists() {
                        if ctx.opts.env.flag("PREWARM") {
                            precompile_go_std(ctx, step, &dest).await?;
                        }
                        return Ok(Out::Tool(Installed {
                            name: "go".into(),
                            version,
                            bin_dir: dest.join("bin"),
                            root: dest,
                            archive: None,
                        }));
                    }
                    let tmp = staging(&dest);
                    acro_toolchain::go::install(fetcher, &version, &tmp).await?;
                    publish(&tmp, &dest)?;
                    acro_events::log(&step.id, format!("go {version}"));
                    if ctx.opts.env.flag("PREWARM") {
                        precompile_go_std(ctx, step, &dest).await?;
                    }
                    Ok(Out::Tool(Installed { name: "go".into(), version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "bun" => {
                    let release = acro_toolchain::bun::resolve(fetcher, spec).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("bun-{}", release.version));
                    let marker = dest.join(".acro-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acro_toolchain::bun::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acro_events::log(&step.id, format!("bun {}", release.version));
                    Ok(Out::Tool(Installed { name: "bun".into(), version: release.version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "yarn-berry" => {
                    let dest = ctx.opts.home.join("toolchains").join(format!("yarn-berry-{spec}"));
                    if !dest.join(".acro-complete").exists() {
                        let tmp = staging(&dest);
                        let url = format!("https://repo.yarnpkg.com/{spec}/packages/yarnpkg-cli/bin/yarn.js");
                        acro_events::log(&step.id, format!("warning: {url} has no published checksum; pinned by version only"));
                        let blob = fetcher.blob("yarn.js", &url, None).await?;
                        std::fs::create_dir_all(&tmp)?;
                        std::fs::copy(&blob.path, tmp.join("yarn.js"))?;
                        publish(&tmp, &dest)?;
                    }
                    Ok(Out::Tool(Installed { name: "yarn-berry".into(), version: spec.clone(), bin_dir: dest.clone(), root: dest, archive: None }))
                }
                "composer" => {
                    let dest = ctx.opts.home.join("toolchains").join("composer-latest-stable");
                    if !dest.join(".acro-complete").exists() {
                        let tmp = staging(&dest);
                        let sum = fetcher.bytes("https://getcomposer.org/download/latest-stable/composer.phar.sha256sum").await?;
                        let hex = String::from_utf8_lossy(&sum).split_whitespace().next().unwrap_or("").to_string();
                        let expected = acro_store::Integrity::parse_hex(acro_store::Algo::Sha256, &hex)?;
                        let blob = fetcher.blob("composer.phar", "https://getcomposer.org/download/latest-stable/composer.phar", Some(expected)).await?;
                        std::fs::create_dir_all(tmp.join("bin"))?;
                        std::fs::copy(&blob.path, tmp.join("bin/composer"))?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(tmp.join("bin/composer"), std::fs::Permissions::from_mode(0o755))?;
                        publish(&tmp, &dest)?;
                    }
                    Ok(Out::Tool(Installed { name: "composer".into(), version: "latest-stable".into(), bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "mise" => {
                    let tag = if spec.is_empty() || spec == "latest" { github_latest_tag("jdx/mise").await? } else { format!("v{}", spec.trim_start_matches('v')) };
                    let dest = ctx.opts.home.join("toolchains").join(format!("mise-{tag}"));
                    if !dest.join(".acro-complete").exists() {
                        let tmp = staging(&dest);
                        let arch = if std::env::consts::ARCH == "aarch64" { "arm64" } else { "x64" };
                        let file = format!("mise-{tag}-linux-{arch}-musl");
                        let base = format!("https://github.com/jdx/mise/releases/download/{tag}");
                        let sums = String::from_utf8_lossy(&fetcher.bytes(&format!("{base}/SHASUMS256.txt")).await?).into_owned();
                        let expected = acro_toolchain::parse_shasums(&sums.replace("./", ""), &file)?;
                        let blob = fetcher.blob(&file, &format!("{base}/{file}"), Some(expected)).await?;
                        std::fs::create_dir_all(tmp.join("bin"))?;
                        std::fs::copy(&blob.path, tmp.join("bin/mise"))?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(tmp.join("bin/mise"), std::fs::Permissions::from_mode(0o755))?;
                        publish(&tmp, &dest)?;
                    }
                    acro_events::log(&step.id, format!("mise {tag}"));
                    Ok(Out::Tool(Installed { name: "mise".into(), version: tag, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                t if t.starts_with("npm:") => {
                    let pkg = &t[4..];
                    let release = acro_toolchain::npmpkg::resolve(fetcher, pkg, spec).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("npm-{}-{}", pkg.replace('/', "+"), release.version));
                    if !dest.join(".acro-complete").exists() {
                        let tmp = staging(&dest);
                        acro_toolchain::npmpkg::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acro_events::log(&step.id, format!("{pkg} {}", release.version));
                    Ok(Out::Tool(Installed { name: t.to_string(), version: release.version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "uv" => {
                    let release = acro_toolchain::uv::resolve(fetcher, spec).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("uv-{}", release.version));
                    let marker = dest.join(".acro-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acro_toolchain::uv::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acro_events::log(&step.id, format!("uv {}", release.version));
                    Ok(Out::Tool(Installed { name: "uv".into(), version: release.version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "python-standalone" => {
                    let key = if spec.is_empty() { "latest".to_string() } else { spec.clone() };
                    let dest = ctx.opts.home.join("toolchains").join(format!("python-standalone-{key}"));
                    if !dest.join(".acro-complete").exists() {
                        let uv = acro_toolchain::uv::resolve(fetcher, "").await?;
                        let uv_dest = ctx.opts.home.join("toolchains").join(format!("uv-{}", uv.version));
                        if !uv_dest.join(".acro-complete").exists() {
                            let tmp = staging(&uv_dest);
                            acro_toolchain::uv::install(fetcher, &uv, &tmp).await?;
                            publish(&tmp, &uv_dest)?;
                        }
                        let tmp = staging(&dest);
                        let install_dir = PathBuf::from(format!("{}.uv", tmp.display()));
                        let _ = std::fs::remove_dir_all(&install_dir);
                        let mut cmd = tokio::process::Command::new(uv_dest.join("bin/uv"));
                        cmd.arg("python").arg("install").arg("--install-dir").arg(&install_dir);
                        if !spec.is_empty() {
                            cmd.arg(spec);
                        }
                        let out = cmd
                            .env("UV_CACHE_DIR", ctx.work.join("uv-cache"))
                            .env("UV_PYTHON_BIN_DIR", install_dir.join(".bin"))
                            .env_remove("UV_PYTHON")
                            .output()
                            .await?;
                        if !out.status.success() {
                            bail!("uv python install {key}: {}", String::from_utf8_lossy(&out.stderr));
                        }
                        let inner = std::fs::read_dir(&install_dir)?
                            .flatten()
                            .map(|e| e.path())
                            .find(|p| {
                                std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
                                    && p.file_name().map(|n| n.to_string_lossy().starts_with("cpython-")).unwrap_or(false)
                            })
                            .ok_or_else(|| anyhow!("uv python install {key} produced no cpython directory"))?;
                        std::fs::rename(&inner, &tmp)?;
                        let _ = std::fs::remove_dir_all(&install_dir);
                        let bin = tmp.join("bin");
                        if !bin.join("python").exists() {
                            let _ = std::os::unix::fs::symlink("python3", bin.join("python"));
                        }
                        publish(&tmp, &dest)?;
                    }
                    let version = std::process::Command::new(dest.join("bin/python3"))
                        .arg("-c")
                        .arg("import platform; print(platform.python_version())")
                        .output()
                        .ok()
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or(key);
                    acro_events::log(&step.id, format!("python {version} (standalone)"));
                    Ok(Out::Tool(Installed { name: "python-standalone".into(), version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                "rust" => {
                    let release = acro_cargo::resolve(fetcher, spec, &[]).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("rust-{}", release.version));
                    let marker = dest.join(".acro-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acro_cargo::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acro_events::log(&step.id, format!("rust {} ({})", release.version, release.date));
                    Ok(Out::Tool(Installed { name: "rust".into(), version: release.version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                other => bail!("unsupported toolchain {other}"),
            }
        }
        Action::NpmFetch { manager, lockfile, dev, .. } => {
            let opts = InstallOptions { include_dev: *dev, include_optional: true, platform: Default::default() };
            let out_of_sync = manager == "npm" && !lockfile.is_empty() && npm_lock_out_of_sync(&ctx.opts.app_dir, lockfile);
            if out_of_sync && ctx.opts.env.flag("STRICT_LOCKFILE") {
                bail!("package-lock.json is out of sync with package.json (ACRO_STRICT_LOCKFILE=1); run `npm install` and commit the lockfile");
            }
            if out_of_sync {
                acro_events::log(&step.id, "warning: package-lock.json is out of sync with package.json; resolving from the registry like `npm install`");
            }
            let plan = if lockfile.is_empty() || out_of_sync {
                let pj = crate::detect::read_package_json(&ctx.opts.app_dir)?;
                let ws = acro_npm::yarn::expand_workspaces(&ctx.opts.app_dir, &pj);
                acro_npm::resolve::plan_without_lockfile(&ctx.fetcher, &pj, &ws, &opts).await?
            } else {
                install_plan_for(manager, &ctx.opts.app_dir, lockfile, &opts)?
            };
            for p in &plan.packages {
                if let acro_npm::Source::Git { url } = &p.source {
                    acro_events::log(&step.id, format!("warning: {} comes from git ({url}); it is pinned by commit, not by content hash", p.path));
                }
            }
            let tarballs = acro_npm::install::fetch_all(&ctx.fetcher, &plan).await?;
            acro_events::log(&step.id, format!("{} packages ({} tarballs)", plan.packages.len(), tarballs.len()));
            Ok(Out::Npm(Arc::new(NpmState { plan, tarballs })))
        }
        Action::CopySource { exclude } => {
            let app = ctx.opts.app_dir.clone();
            let src = ctx.src.clone();
            let ex = with_config_excludes(ctx, exclude);
            let n = tokio::task::spawn_blocking(move || {
                let ig = Ignore::load(&app, &ex);
                source::copy_tree(&app, &src, &ig)
            })
            .await??;
            acro_events::log(&step.id, format!("{:.1} MB", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::NpmInstall { target, scripts, manager, types_only, .. } => {
            let state = ctx
                .dep_outputs(step)
                .into_iter()
                .find_map(|o| if let Out::Npm(s) = o { Some(s) } else { None })
                .ok_or_else(|| anyhow!("install without fetched packages"))?;
            let root = if target == "src" { ctx.src.clone() } else { ctx.work.join(target) };
            std::fs::create_dir_all(&root)?;
            let reuse = ctx.cache.as_ref().filter(|c| target == "src" && ctx.src.starts_with(&c.dir)).map(|c| c.dir.clone());
            let mut versions: Vec<String> =
                ctx.dep_outputs(step).into_iter().filter_map(|o| if let Out::Tool(t) = o { Some(format!("{}={}", t.name, t.version)) } else { None }).collect();
            versions.sort();
            let key = format!("{} {}", step.hash, versions.join(" "));
            if let Some(cache_dir) = &reuse {
                if restore_node_modules(cache_dir, &root, &key) {
                    std::fs::write(cache_dir.join("nm.key"), &key)?;
                    acro_events::log(&step.id, "reused node_modules from the previous build (same lockfile and toolchains)");
                    return Ok(Out::None);
                }
                let prev = cache_dir.join("nm-prev");
                if prev.exists() {
                    let old = cache_dir.join(format!(".nm-old-{}-{}", std::process::id(), step.id));
                    if std::fs::rename(&prev, &old).is_ok() {
                        std::thread::spawn(move || {
                            let _ = std::fs::remove_dir_all(old);
                        });
                    }
                }
            }
            if target != "src" {
                let _ = std::fs::copy(ctx.opts.app_dir.join("package.json"), root.join("package.json"));
            }
            let r2 = root.clone();
            let s2 = state.clone();
            let types = *types_only;
            let n = tokio::task::spawn_blocking(move || acro_npm::install::materialize_with(&s2.plan, &s2.tarballs, &r2, types)).await??;
            acro_events::log(&step.id, format!("{:.1} MB written", n as f64 / 1e6));
            if manager == "pnpm" && !root.join("node_modules/.modules.yaml").exists() {
                std::fs::create_dir_all(root.join("node_modules"))?;
                std::fs::write(
                    root.join("node_modules/.modules.yaml"),
                    "hoistPattern:\n  - '*'\nhoistedDependencies: {}\nincluded:\n  dependencies: true\n  devDependencies: true\n  optionalDependencies: true\nlayoutVersion: 5\nnodeLinker: isolated\npendingBuilds: []\npublicHoistPattern: []\nregistries:\n  default: https://registry.npmjs.org/\nskipped: []\nvirtualStoreDir: .pnpm\n",
                )?;
            }
            if !scripts.is_empty() && scripts != "none" {
                let policy = acro_npm::scripts::Policy::parse(scripts);
                let mut jobs = acro_npm::scripts::lifecycle_jobs(&state.plan, &root, &policy);
                if target == "prod" && ctx.opts.env.flag("NODE_PLAYWRIGHT_INSTALL") && root.join("node_modules/playwright/cli.js").exists() {
                    jobs.push(acro_npm::scripts::ScriptJob {
                        path: "node_modules/playwright".into(),
                        name: "playwright".into(),
                        commands: vec![("install".into(), "node cli.js install --only-shell".into())],
                    });
                }
                run_lifecycle(ctx, step, &root, &jobs).await?;
            }
            if let Some(cache_dir) = &reuse {
                std::fs::write(cache_dir.join("nm.key"), &key)?;
            }
            Ok(Out::None)
        }
        Action::GoModules { .. } => {
            let text = std::fs::read_to_string(ctx.opts.app_dir.join("go.sum"))?;
            let sum = acro_gomod::parse_go_sum(&text)?;
            let cache = acro_gomod::ModCache { root: ctx.cache_path("gomodcache") };
            let proxy = ctx
                .opts
                .env
                .vars
                .get("GOPROXY")
                .map(|p| p.split(',').next().unwrap_or("").to_string())
                .filter(|p| p.starts_with("http"))
                .unwrap_or_else(|| "https://proxy.golang.org".to_string());
            let stats = acro_gomod::download_all(&ctx.fetcher, &sum, &cache, &proxy).await?;
            acro_events::log(&step.id, format!("{} modules, {} go.mod files", stats.modules, stats.gomods));
            Ok(Out::None)
        }
        Action::BundleSpa { manager, lockfile, out } => {
            let opts = InstallOptions { include_dev: true, include_optional: true, platform: Default::default() };
            let plan = install_plan_for(manager, &ctx.opts.app_dir, lockfile, &opts)?;
            let root = ctx.work.join("bundle");
            let app = ctx.opts.app_dir.clone();
            let r2 = root.clone();
            tokio::task::spawn_blocking(move || source::copy_tree(&app, &r2, &Ignore::load(&app, &["**/node_modules".to_string(), "dist".to_string()]))).await??;
            let mut env = BTreeMap::new();
            for (k, v) in &ctx.opts.env.vars {
                env.insert(k.clone(), v.clone());
            }
            let res = acro_bundle::build_spa(acro_bundle::SpaInput {
                root: root.clone(),
                out_dir: root.join(out),
                plan: Arc::new(plan),
                fetcher: ctx.fetcher.clone(),
                base: "/".into(),
                env,
            })
            .await?;
            acro_events::log(
                &step.id,
                format!(
                    "{} of {} packages fetched ({:.1} MB, last at {} ms, extract {} ms), {} files in {} ms",
                    res.stats.packages_fetched,
                    res.stats.packages_in_lockfile,
                    res.stats.bytes_fetched as f64 / 1e6,
                    res.stats.last_fetch_ms,
                    res.stats.extract_ms,
                    res.files,
                    res.ms
                ),
            );
            Ok(Out::None)
        }
        Action::CargoVendor { .. } => {
            let text = std::fs::read_to_string(ctx.opts.app_dir.join("Cargo.lock"))?;
            let lock = acro_cargo::parse_lock(&text)?;
            let vendor_dir = ctx.cache_path("cargo-vendor");
            let stats = acro_cargo::vendor(&ctx.fetcher, &lock, &vendor_dir).await?;
            let cargo_home = ctx.cache_path("cargo-home");
            std::fs::create_dir_all(&cargo_home)?;
            std::fs::write(cargo_home.join("config.toml"), acro_cargo::cargo_config(&vendor_dir))?;
            acro_events::log(&step.id, format!("{} crates, {:.1} MB", stats.crates, stats.bytes as f64 / 1e6));
            Ok(Out::None)
        }
        Action::Run { argv, env, network, cwd } => {
            let tools = ctx.tools();
            let mut path: Vec<String> = Vec::new();
            let cwd_path = match cwd.as_str() {
                "@app" => ctx.opts.app_dir.clone(),
                "." => ctx.src.clone(),
                other => ctx.src.join(other),
            };
            let nm_bin = cwd_path.join("node_modules").join(".bin");
            if nm_bin.exists() {
                path.push(nm_bin.to_string_lossy().into_owned());
            }
            for t in &tools {
                path.push(t.bin_dir.to_string_lossy().into_owned());
            }
            path.push("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            let home = ctx.work.join("home");
            let tmp = ctx.work.join("tmp");
            std::fs::create_dir_all(&home)?;
            std::fs::create_dir_all(&tmp)?;
            std::fs::create_dir_all(ctx.work.join("out"))?;
            let mut full_env: BTreeMap<String, String> = BTreeMap::new();
            full_env.insert("PATH".into(), path.join(":"));
            full_env.insert("HOME".into(), home.to_string_lossy().into_owned());
            full_env.insert("TMPDIR".into(), tmp.to_string_lossy().into_owned());
            full_env.insert("LANG".into(), "C.UTF-8".into());
            full_env.insert("SOURCE_DATE_EPOCH".into(), "0".into());
            let pnpm_hoisted = ctx.src.join("node_modules/.pnpm/node_modules");
            if pnpm_hoisted.is_dir() {
                full_env.insert("NODE_PATH".into(), pnpm_hoisted.to_string_lossy().into_owned());
            }
            if let Some(go) = tools.iter().find(|t| t.name == "go") {
                if env.get("CGO_ENABLED").map(|v| v == "0").unwrap_or(false) {
                    seed_go_cache(&go.root, &ctx.cache_path("gocache"));
                }
                full_env.insert("GOPATH".into(), ctx.work.join("gopath").to_string_lossy().into_owned());
                full_env.insert("GOMODCACHE".into(), ctx.cache_path("gomodcache").to_string_lossy().into_owned());
                full_env.insert("GOCACHE".into(), ctx.cache_path("gocache").to_string_lossy().into_owned());
                full_env.insert("GOPROXY".into(), "off".into());
                full_env.insert("GOSUMDB".into(), "off".into());
                full_env.insert("GOTOOLCHAIN".into(), "local".into());
                full_env.insert("GOFLAGS".into(), "-mod=readonly".into());
            }
            if tools.iter().any(|t| t.name == "rust") {
                let cargo_home = ctx.cache_path("cargo-home");
                std::fs::create_dir_all(&cargo_home)?;
                full_env.insert("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned());
                full_env.insert("CARGO_TARGET_DIR".into(), ctx.cache_path("cargo-target").to_string_lossy().into_owned());
                full_env.insert("CARGO_TERM_COLOR".into(), "never".into());
                full_env.insert("CARGO_INCREMENTAL".into(), "0".into());
            }
            if let Some(node) = tools.iter().find(|t| t.name == "node") {
                let global = home.join(".npm-global");
                full_env.insert("npm_config_prefix".into(), global.to_string_lossy().into_owned());
                if let Some(p) = full_env.get_mut("PATH") {
                    *p = format!("{}:{p}", global.join("bin").display());
                }
                full_env.insert("npm_node_execpath".into(), node.bin_dir.join("node").to_string_lossy().into_owned());
                full_env.insert("INIT_CWD".into(), cwd_path.to_string_lossy().into_owned());
            }
            for (k, v) in env {
                full_env.insert(k.clone(), ctx.subst(v));
            }
            if env.contains_key("ACRO_NFT_CACHE") {
                std::fs::write(ctx.work.join("acro-nft-cache.js"), NFT_CACHE_HOOK)?;
            }
            let argv: Vec<String> = argv.iter().map(|a| ctx.subst(a)).collect();
            let goflags_vendor = argv.iter().any(|a| a == "-mod=vendor");
            if goflags_vendor {
                full_env.remove("GOFLAGS");
            }
            let node_cache = ctx
                .cache
                .as_ref()
                .filter(|_| (cwd_path.starts_with(&ctx.src) || cwd_path.starts_with(&ctx.work)) && cwd_path.join("package.json").exists())
                .map(|c| c.dir.clone());
            if let Some(c) = &node_cache {
                restore_node_caches(c, &cwd_path);
            }
            let res = ctx.exec.run(Cmd { step: step.id.clone(), argv, cwd: cwd_path.clone(), env: full_env, network: *network }).await;
            if let Some(c) = &node_cache {
                save_node_caches(c, &cwd_path);
            }
            res?;
            Ok(Out::None)
        }
        Action::ImageRun { image, commands, env, network, mount_app, after, tools, lowers } => {
            let image = if image == "@base" {
                ctx.base().map(|b| b.reference.to_string()).ok_or_else(|| anyhow!("no base image resolved"))?
            } else if let Some(variant) = image.strip_prefix("@base-variant:") {
                let (from, to) = variant.split_once('=').ok_or_else(|| anyhow!("invalid image alias {image}"))?;
                let base = ctx.base().ok_or_else(|| anyhow!("no base image resolved"))?;
                let mut r = base.reference.clone();
                r.digest = None;
                r.tag = r.tag.map(|t| t.replacen(from, to, 1));
                r.to_string()
            } else {
                resolve_alias(ctx, image).await?
            };
            let (mut lower, image_env) = image_rootfs(ctx, &image).await?;
            for prev in after.iter().chain(lowers.iter()) {
                match ctx.get(prev) {
                    Out::Upper(p) => lower.push(p),
                    _ => bail!("step {prev} has no filesystem output"),
                }
            }
            let base = ctx.work.join(format!("run-{}", step.id));
            let upper = base.join("upper");
            let mut full_env: BTreeMap<String, String> = image_env;
            full_env.entry("PATH".into()).or_insert_with(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            full_env.insert("HOME".into(), "/root".into());
            full_env.insert("SOURCE_DATE_EPOCH".into(), "0".into());
            for (k, v) in env {
                let v = ctx.subst(v);
                let placeholder = format!("${{{k}}}");
                let v = if v.contains(&placeholder) { v.replace(&placeholder, full_env.get(k).map(|s| s.as_str()).unwrap_or("")) } else { v };
                full_env.insert(k.clone(), v);
            }
            let mut binds = Vec::new();
            let installed = ctx.tools();
            let mut extra_path = Vec::new();
            for t in tools {
                let inst = installed.iter().find(|i| &i.name == t).ok_or_else(|| anyhow!("toolchain {t} not installed"))?;
                let root = format!("/opt/acro/{}", t.replace(':', "-"));
                let rel = inst.bin_dir.strip_prefix(&inst.root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "bin".into());
                binds.push(acro_exec::rootfs::Bind { host: inst.root.clone(), guest: root.clone(), readonly: true });
                extra_path.push(format!("{root}/{rel}"));
            }
            if !extra_path.is_empty() {
                let p = full_env.get("PATH").cloned().unwrap_or_default();
                full_env.insert("PATH".into(), format!("{}:{p}", extra_path.join(":")));
            }
            if *mount_app {
                std::fs::create_dir_all(&ctx.src)?;
                binds.push(acro_exec::rootfs::Bind { host: ctx.src.clone(), guest: "/app".into(), readonly: false });
                if let Some(c) = &ctx.cache {
                    for (name, guest) in [
                        ("root-cache", "/root/.cache"),
                        ("m2", "/root/.m2"),
                        ("gradle", "/root/.gradle"),
                        ("nuget", "/root/.nuget"),
                        ("hex", "/root/.hex"),
                        ("mix", "/root/.mix"),
                        ("gem-cache", "/usr/local/bundle/cache"),
                    ] {
                        let host = c.dir.join(name);
                        std::fs::create_dir_all(&host)?;
                        binds.push(acro_exec::rootfs::Bind { host, guest: guest.into(), readonly: false });
                    }
                    if full_env.contains_key("UV_CACHE_DIR") {
                        full_env.insert("UV_CACHE_DIR".into(), "/root/.cache/uv".into());
                    }
                    full_env.entry("COMPOSER_CACHE_DIR".into()).or_insert_with(|| "/root/.cache/composer".into());
                    full_env.entry("PIP_CACHE_DIR".into()).or_insert_with(|| "/root/.cache/pip".into());
                }
            }
            let spec = acro_exec::rootfs::RootfsRun {
                step: step.id.clone(),
                lower,
                upper: upper.clone(),
                work: base.join("work"),
                merged: base.join("merged"),
                binds,
                argv: vec!["/bin/sh".into(), "-c".into(), commands.join(" && ")],
                env: full_env,
                cwd: if *mount_app { "/app".into() } else { "/".into() },
                network: *network,
            };
            acro_exec::rootfs::run(spec).await?;
            Ok(Out::Upper(upper))
        }
        Action::Layer { dest, from } => {
            let layer = build_layer(ctx, step, dest, from).await?;
            acro_events::log(
                &step.id,
                format!(
                    "{} {:.1} MB -> {:.1} MB",
                    &layer.digest.hex()[..12],
                    layer.uncompressed_size as f64 / 1e6,
                    layer.size as f64 / 1e6
                ),
            );
            if let Some(target) = &ctx.opts.target
                && ctx.plan.image.layers.iter().any(|l| l == &step.id)
            {
                assemble::push_layers(&ctx.registry, target, std::slice::from_ref(&layer)).await?;
            }
            Ok(Out::Layer(layer))
        }
        Action::Push => {
            let base = ctx.base();
            let layers: Vec<Layer> = ctx
                .plan
                .image
                .layers
                .iter()
                .map(|id| match ctx.get(id) {
                    Out::Layer(l) => Ok(l),
                    _ => Err(anyhow!("layer {id} missing")),
                })
                .collect::<Result<_>>()?;
            let spec = &ctx.plan.image;
            let mut labels = BTreeMap::new();
            labels.insert("dev.acropolis.plan".to_string(), ctx.plan.hash.clone());
            let patch = ConfigPatch {
                env: spec.env.iter().map(|(k, v)| (k.clone(), ctx.subst_versions(v))).collect(),
                entrypoint: spec.entrypoint.clone(),
                cmd: spec.cmd.clone(),
                workdir: spec.workdir.clone(),
                user: spec.user.clone(),
                exposed_ports: spec.ports.clone(),
                labels,
            };
            let assembled = assemble::assemble(base.as_deref(), &layers, &patch)?;
            if let Some(out) = &ctx.opts.oci_out {
                write_oci_layout(ctx, base.as_deref(), &layers, &assembled, out).await?;
                acro_events::log(&step.id, format!("wrote OCI layout {}", out.display()));
            }
            if let Some(target) = &ctx.opts.target {
                assemble::push_layers(&ctx.registry, target, &layers).await?;
                let digest = assemble::push_manifest(&ctx.registry, target, &assembled).await?;
                acro_events::emit(acro_events::Event::ImagePushed { reference: target.to_string(), digest: digest.clone() });
                Ok(Out::Pushed(digest))
            } else {
                ctx.store.put_bytes("manifest", &assembled.manifest, None)?;
                ctx.store.put_bytes("config", &assembled.config, None)?;
                Ok(Out::Pushed(assembled.manifest_digest))
            }
        }
    }
}

pub fn install_plan_for(manager: &str, app_dir: &Path, lockfile: &str, opts: &InstallOptions) -> Result<InstallPlan> {
    let bytes = std::fs::read(app_dir.join(lockfile)).with_context(|| format!("reading {lockfile}"))?;
    match manager {
        "npm" => InstallPlan::from_lock(&PackageLock::parse(&bytes)?, opts),
        "pnpm" => {
            let text = String::from_utf8(bytes).context("pnpm-lock.yaml is not UTF-8")?;
            acro_npm::pnpm::PnpmLock::parse(&text)?.install_plan(opts, &[])
        }
        "yarn" => {
            let text = String::from_utf8(bytes).context("yarn.lock is not UTF-8")?;
            let pj: serde_json::Value = serde_json::from_slice(&std::fs::read(app_dir.join("package.json"))?)?;
            let ws = acro_npm::yarn::expand_workspaces(app_dir, &pj);
            acro_npm::yarn::YarnLock::parse(&text)?.install_plan(&pj, &ws, opts)
        }
        "bun" => {
            let text = String::from_utf8(bytes).context("bun.lock is not UTF-8")?;
            acro_npm::bun::BunLock::parse(&text)?.install_plan(opts)
        }
        other => bail!("unsupported package manager {other}"),
    }
}

async fn run_lifecycle(ctx: &Arc<Ctx>, step: &Step, root: &Path, jobs: &[acro_npm::scripts::ScriptJob]) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let tools = ctx.tools();
    let node = tools.iter().find(|t| t.name == "node").ok_or_else(|| anyhow!("install scripts need the node toolchain"))?;
    let npm_dir = node.root.join("lib/node_modules/npm");
    let home = ctx.work.join("home");
    std::fs::create_dir_all(&home)?;
    for job in jobs {
        let pkg_dir = root.join(&job.path);
        let version = std::fs::read(pkg_dir.join("package.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        for (stage, command) in &job.commands {
            let mut path = vec![
                pkg_dir.join("node_modules/.bin").to_string_lossy().into_owned(),
                root.join("node_modules/.bin").to_string_lossy().into_owned(),
                npm_dir.join("node_modules/@npmcli/run-script/lib/node-gyp-bin").to_string_lossy().into_owned(),
                npm_dir.join("bin/node-gyp-bin").to_string_lossy().into_owned(),
            ];
            for t in &tools {
                path.push(t.bin_dir.to_string_lossy().into_owned());
            }
            path.push("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            let mut env = BTreeMap::new();
            env.insert("PATH".to_string(), path.join(":"));
            env.insert("HOME".to_string(), home.to_string_lossy().into_owned());
            env.insert("npm_lifecycle_event".to_string(), stage.clone());
            env.insert("npm_lifecycle_script".to_string(), command.clone());
            env.insert("npm_package_name".to_string(), job.name.clone());
            env.insert("npm_package_version".to_string(), version.clone());
            env.insert("npm_config_nodedir".to_string(), node.root.to_string_lossy().into_owned());
            env.insert("npm_config_node_gyp".to_string(), npm_dir.join("node_modules/node-gyp/bin/node-gyp.js").to_string_lossy().into_owned());
            env.insert("npm_config_user_agent".to_string(), format!("npm/10 node/v{} linux x64", node.version));
            env.insert("npm_node_execpath".to_string(), node.bin_dir.join("node").to_string_lossy().into_owned());
            env.insert("INIT_CWD".to_string(), root.to_string_lossy().into_owned());
            env.insert("PUPPETEER_CACHE_DIR".to_string(), root.join("node_modules/.cache/puppeteer").to_string_lossy().into_owned());
            env.insert("PLAYWRIGHT_BROWSERS_PATH".to_string(), root.join("node_modules/.cache/ms-playwright").to_string_lossy().into_owned());
            env.insert("NODE_ENV".to_string(), "production".to_string());
            acro_events::log(&step.id, format!("{} {stage}: {command}", job.name));
            ctx.exec
                .run(Cmd {
                    step: step.id.clone(),
                    argv: vec!["/bin/sh".into(), "-c".into(), command.clone()],
                    cwd: pkg_dir.clone(),
                    env,
                    network: true,
                })
                .await
                .with_context(|| format!("{stage} script of {}", job.name))?;
        }
    }
    Ok(())
}

async fn resolve_alias(ctx: &Arc<Ctx>, image: &str) -> Result<String> {
    let Some(build_image) = image.strip_prefix("@slim-of:") else { return Ok(image.to_string()) };
    let (layers, _) = image_rootfs(ctx, build_image).await?;
    for dir in layers.iter().rev() {
        for rel in ["etc/os-release", "usr/lib/os-release"] {
            let Ok(text) = std::fs::read_to_string(dir.join(rel)) else { continue };
            let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim_matches('"').to_string());
            if field("ID=").as_deref() == Some("debian")
                && let Some(code) = field("VERSION_CODENAME=").filter(|c| !c.is_empty())
            {
                return Ok(format!("debian:{code}-slim"));
            }
        }
    }
    bail!("could not determine the Debian release of {build_image} for a slim runtime image")
}

async fn image_rootfs(ctx: &Arc<Ctx>, image: &str) -> Result<(Vec<PathBuf>, BTreeMap<String, String>)> {
    let r = Reference::parse(image)?;
    let resolved = ctx.registry.resolve(&r, &ctx.opts.platform).await?;
    let mut env = BTreeMap::new();
    if let Some(list) = resolved.config.get("config").and_then(|c| c.get("Env")).and_then(|e| e.as_array()) {
        for item in list {
            if let Some((k, v)) = item.as_str().and_then(|s| s.split_once('=')) {
                env.insert(k.to_string(), v.to_string());
            }
        }
    }
    let futs = resolved.manifest.layers.iter().map(|d| {
        let ctx = ctx.clone();
        let reference = resolved.reference.clone();
        let d = d.clone();
        async move {
            let hex = d.digest.trim_start_matches("sha256:").to_string();
            let rootfs_root = std::env::var("ACRO_ROOTFS").map(PathBuf::from).unwrap_or_else(|_| ctx.opts.home.join("rootfs"));
            std::fs::create_dir_all(&rootfs_root)?;
            let dir = rootfs_root.join(&hex);
            let marker = rootfs_root.join(format!("{hex}.complete"));
            if marker.exists() {
                crate::gc::touch(&marker);
                return Ok::<PathBuf, anyhow::Error>(dir);
            }
            let staged = staging(&dir);
            let (url, headers) = ctx.registry.blob_location(&reference, &d.digest).await?;
            let expected = acro_store::Integrity::parse_oci(&d.digest)?;
            let mt = d.media_type.clone();
            let dir2 = staged.clone();
            let (_, stored) = ctx
                .fetcher
                .blob_streaming(&d.digest, &url, &headers, Some(expected), move |r| {
                    let reader: Box<dyn std::io::Read + '_> = if mt.contains("zstd") {
                        Box::new(zstd::stream::read::Decoder::new(r)?)
                    } else if mt.contains("gzip") {
                        Box::new(flate2::read::MultiGzDecoder::new(r))
                    } else {
                        Box::new(std::io::Read::take(r, u64::MAX))
                    };
                    acro_oci::unpack::unpack_for_overlay(reader, &dir2)
                })
                .await?;
            if std::fs::rename(&staged, &dir).is_err() {
                let _ = std::fs::remove_dir_all(&staged);
            }
            std::fs::write(&marker, "")?;
            let _ = std::fs::remove_file(&stored.path);
            let _ = std::fs::remove_file(stored.path.with_extension("sha256"));
            Ok(dir)
        }
    });
    let dirs = futures::future::try_join_all(futs).await?;
    Ok((dirs, env))
}

fn npm_lock_out_of_sync(app_dir: &Path, lockfile: &str) -> bool {
    let Ok(pj) = crate::detect::read_package_json(app_dir) else { return false };
    let Ok(bytes) = std::fs::read(app_dir.join(lockfile)) else { return false };
    let Ok(lock) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false };
    let root = lock.get("packages").and_then(|p| p.get(""));
    for k in ["dependencies", "devDependencies", "optionalDependencies"] {
        let want = pj.get(k).and_then(|d| d.as_object()).cloned().unwrap_or_default();
        let have = root.and_then(|r| r.get(k)).and_then(|d| d.as_object()).cloned().unwrap_or_default();
        for (name, range) in &want {
            if have.get(name) != Some(range) {
                return true;
            }
        }
    }
    false
}

fn with_config_excludes(ctx: &Ctx, base: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    if let Some(extra) = ctx.opts.env.vars.get("ACRO_EXCLUDE") {
        out.extend(extra.lines().map(|l| l.to_string()));
    }
    out
}

fn staging(dest: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dest.with_file_name(format!(".{name}.staging-{}-{n}", std::process::id()))
}

const GO_STD_CACHE: &str = ".acro-std-cgo0";

async fn precompile_go_std(ctx: &Arc<Ctx>, step: &Step, root: &Path) -> Result<()> {
    let done = root.join(GO_STD_CACHE).join(".complete");
    if done.exists() {
        return Ok(());
    }
    let staged = staging(&root.join(GO_STD_CACHE));
    let _ = std::fs::remove_dir_all(&staged);
    std::fs::create_dir_all(&staged)?;
    let home = ctx.work.join("prewarm-home");
    std::fs::create_dir_all(&home)?;
    let started = Instant::now();
    let out = tokio::process::Command::new(root.join("bin/go"))
        .args(["build", "-trimpath", "std"])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()))
        .env("HOME", &home)
        .env("GOCACHE", &staged)
        .env("GOROOT", root)
        .env("CGO_ENABLED", "0")
        .env("GOTOOLCHAIN", "local")
        .env("GOFLAGS", "")
        .current_dir(&home)
        .output()
        .await?;
    if !out.status.success() {
        bail!("precompiling the Go standard library failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    std::fs::write(staged.join(".complete"), "")?;
    if std::fs::rename(&staged, root.join(GO_STD_CACHE)).is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    acro_events::log(&step.id, format!("precompiled the Go standard library in {:.1}s", started.elapsed().as_secs_f64()));
    Ok(())
}

fn seed_go_cache(go_root: &Path, gocache: &Path) {
    let std_cache = go_root.join(GO_STD_CACHE);
    if !std_cache.join(".complete").exists() {
        return;
    }
    let empty = std::fs::read_dir(gocache).map(|mut rd| rd.next().is_none()).unwrap_or(true);
    if !empty {
        return;
    }
    let ig = crate::ignore::Ignore::new(&[".complete".to_string()]);
    let _ = source::copy_tree(&std_cache, gocache, &ig);
}

fn publish(staged: &Path, dest: &Path) -> Result<()> {
    std::fs::write(staged.join(".acro-complete"), "")?;
    for _ in 0..3 {
        if dest.join(".acro-complete").exists() {
            let _ = std::fs::remove_dir_all(staged);
            return Ok(());
        }
        if dest.exists() {
            let stale = staging(dest).with_extension("stale");
            if std::fs::rename(dest, &stale).is_ok() {
                std::thread::spawn(move || {
                    let _ = std::fs::remove_dir_all(stale);
                });
            }
        }
        match std::fs::rename(staged, dest) {
            Ok(()) => return Ok(()),
            Err(_) if dest.join(".acro-complete").exists() => {
                let _ = std::fs::remove_dir_all(staged);
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    bail!("could not publish {}", dest.display())
}

fn image_cache_path(ctx: &Ctx, image: &str) -> PathBuf {
    let key = acro_store::sha256_bytes(format!("{image}|{:?}", ctx.opts.platform).as_bytes()).hex();
    ctx.opts.home.join("cache").join("images").join(&key[..24])
}

fn load_cached_image(ctx: &Ctx, image: &str) -> Option<ResolvedImage> {
    let base = image_cache_path(ctx, image);
    let meta = std::fs::metadata(base.with_extension("json")).ok()?;
    let ttl = ctx.opts.env.config("TAG_TTL").and_then(|(v, _)| v.parse::<u64>().ok()).unwrap_or(900);
    let pinned = image.contains("@sha256:");
    let age = meta.modified().ok()?.elapsed().ok()?;
    if !pinned && (ttl == 0 || age.as_secs() > ttl) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(base.with_extension("json")).ok()?).ok()?;
    let config_raw = bytes::Bytes::from(std::fs::read(base.with_extension("config")).ok()?);
    let config_digest = acro_store::sha256_bytes(&config_raw).to_oci();
    let manifest: acro_oci::image::Manifest = serde_json::from_value(v.get("manifest")?.clone()).ok()?;
    if manifest.config.digest != config_digest {
        return None;
    }
    Some(ResolvedImage {
        reference: Reference::parse(v.get("reference")?.as_str()?).ok()?,
        manifest,
        manifest_digest: v.get("digest")?.as_str()?.to_string(),
        config: serde_json::from_slice(&config_raw).ok()?,
        config_raw,
    })
}

fn save_cached_image(ctx: &Ctx, image: &str, r: &ResolvedImage) {
    let base = image_cache_path(ctx, image);
    if let Some(parent) = base.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let v = serde_json::json!({ "reference": r.reference.to_string(), "digest": r.manifest_digest, "manifest": r.manifest });
    let _ = std::fs::write(base.with_extension("config"), &r.config_raw);
    if let Ok(bytes) = serde_json::to_vec(&v) {
        let tmp = base.with_extension(format!("json.{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, base.with_extension("json"));
        }
    }
}

async fn resolve_base(ctx: &Arc<Ctx>, step: &Step, image: &str) -> Result<Out> {
    if let Some(resolved) = load_cached_image(ctx, image) {
        if let Some(target) = ctx.opts.target.clone() {
            let reg = ctx.registry.clone();
            let src = resolved.reference.clone();
            let layers = resolved.manifest.layers.clone();
            let handle = tokio::spawn(async move { assemble::copy_layers(&reg, &src, &layers, &target).await });
            *ctx.base_copy.lock().unwrap() = Some(handle);
        }
        acro_events::log(&step.id, format!("{} -> {} (cached)", image, resolved.manifest_digest));
        return Ok(Out::Base(Arc::new(resolved)));
    }
    let r = Reference::parse(image)?;
    let (reference, manifest, digest) = match ctx.registry.resolve_manifest(&r, &ctx.opts.platform).await {
        Err(e) if image.contains("-trixie-slim") && format!("{e:#}").contains("MANIFEST_UNKNOWN") => {
            let fallback = image.replace("-trixie-slim", "-bookworm-slim");
            acro_events::log(&step.id, format!("warning: {image} does not exist, using {fallback}"));
            ctx.registry.resolve_manifest(&Reference::parse(&fallback)?, &ctx.opts.platform).await?
        }
        other => other?,
    };
    if let Some(target) = ctx.opts.target.clone() {
        let reg = ctx.registry.clone();
        let src = reference.clone();
        let layers = manifest.layers.clone();
        let handle = tokio::spawn(async move { assemble::copy_layers(&reg, &src, &layers, &target).await });
        *ctx.base_copy.lock().unwrap() = Some(handle);
    }
    let resolved = ctx.registry.with_config(reference, manifest, digest).await?;
    save_cached_image(ctx, image, &resolved);
    acro_events::log(&step.id, format!("{} -> {}", image, resolved.manifest_digest));
    Ok(Out::Base(Arc::new(resolved)))
}

async fn write_oci_layout(ctx: &Arc<Ctx>, base: Option<&ResolvedImage>, layers: &[Layer], a: &assemble::Assembled, out: &Path) -> Result<()> {
    let mut blobs: Vec<(String, PathBuf)> = Vec::new();
    if let Some(b) = base {
        let futs = b.manifest.layers.iter().map(|d| async move {
            let (url, headers) = ctx.registry.blob_location(&b.reference, &d.digest).await?;
            let expected = acro_store::Integrity::parse_oci(&d.digest)?;
            let blob = ctx.fetcher.blob_with(&d.digest, &url, &headers, Some(expected)).await?;
            Ok::<_, anyhow::Error>((d.digest.trim_start_matches("sha256:").to_string(), blob.path))
        });
        blobs.extend(futures::future::try_join_all(futs).await?);
    }
    for l in layers {
        blobs.push((l.digest.hex(), l.path.clone()));
    }
    let mut desc = serde_json::json!({
        "mediaType": acro_oci::image::MT_OCI_MANIFEST,
        "digest": a.manifest_digest,
        "size": a.manifest.len(),
    });
    if let Some(t) = &ctx.opts.target {
        desc["annotations"] = serde_json::json!({ "org.opencontainers.image.ref.name": t.tag.clone().unwrap_or_else(|| "latest".into()), "io.containerd.image.name": t.to_string() });
    }
    let index = serde_json::json!({ "schemaVersion": 2, "mediaType": acro_oci::image::MT_OCI_INDEX, "manifests": [desc] });
    let manifest = a.manifest.clone();
    let config = a.config.clone();
    let manifest_hex = a.manifest_digest.trim_start_matches("sha256:").to_string();
    let config_hex = a.config_digest.trim_start_matches("sha256:").to_string();
    let out = out.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = out.with_extension("tmp");
        let file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        let mut tw = TarWriter::new(file);
        tw.dir("blobs", 0o755)?;
        tw.dir("blobs/sha256", 0o755)?;
        tw.file_bytes("oci-layout", 0o644, br#"{"imageLayoutVersion":"1.0.0"}"#)?;
        tw.file_bytes("index.json", 0o644, serde_json::to_vec(&index)?.as_slice())?;
        tw.file_bytes(&format!("blobs/sha256/{manifest_hex}"), 0o644, &manifest)?;
        tw.file_bytes(&format!("blobs/sha256/{config_hex}"), 0o644, &config)?;
        let mut seen = std::collections::HashSet::new();
        for (hex, path) in blobs {
            if !seen.insert(hex.clone()) {
                continue;
            }
            let mut f = std::fs::File::open(&path).with_context(|| format!("opening blob {}", path.display()))?;
            let size = f.metadata()?.len();
            tw.file_reader(&format!("blobs/sha256/{hex}"), 0o644, size, &mut f)?;
        }
        let mut w = tw.finish()?;
        std::io::Write::flush(&mut w)?;
        drop(w);
        std::fs::rename(&tmp, &out)?;
        Ok(())
    })
    .await??;
    Ok(())
}

const NFT_CACHE_HOOK: &str = include_str!("nft-cache.js");

async fn github_latest_tag(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
    acro_events::add_request();
    let resp = client.get(&url).send().await?;
    let loc = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("{url} did not redirect to a release"))?;
    Ok(loc.rsplit('/').next().unwrap_or("").to_string())
}

async fn build_layer(ctx: &Arc<Ctx>, step: &Step, dest: &str, from: &LayerFrom) -> Result<Layer> {
    let opts = ctx.opts.layer;
    let store = ctx.store.clone();
    let comment = step.id.clone();
    let dest = dest.trim_matches('/').to_string();
    match from {
        LayerFrom::AppSource { exclude } => {
            let app = ctx.opts.app_dir.clone();
            let ex = with_config_excludes(ctx, exclude);
            tokio::task::spawn_blocking(move || {
                let ig = Ignore::load(&app, &ex);
                source::dir_layer(&store, &app, &ig, &dest, &comment, opts)
            })
            .await?
        }
        LayerFrom::AppSubdir { path, exclude } => {
            let root = ctx.opts.app_dir.join(path);
            if !root.exists() {
                bail!("{} does not exist", root.display());
            }
            let ex = exclude.clone();
            tokio::task::spawn_blocking(move || {
                let ig = Ignore::new(&ex);
                source::dir_layer(&store, &root, &ig, &dest, &comment, opts)
            })
            .await?
        }
        LayerFrom::WorkDir { path, exclude } => {
            let root = if path == "." {
                ctx.src.clone()
            } else if let Some(rest) = path.strip_prefix("@work/") {
                ctx.work.join(rest)
            } else {
                ctx.src.join(path)
            };
            if !root.exists() {
                bail!("build output {} does not exist", root.display());
            }
            let ex = exclude.clone();
            tokio::task::spawn_blocking(move || {
                let ig = Ignore::new(&ex);
                source::dir_layer(&store, &root, &ig, &dest, &comment, opts)
            })
            .await?
        }
        LayerFrom::Paths { items } => {
            let src = ctx.src.clone();
            let items = items.clone();
            tokio::task::spawn_blocking(move || {
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                let mut head = TarWriter::new(Vec::new());
                for d in source::ancestors(&dest) {
                    head.dir(&d, 0o755)?;
                }
                std::io::Write::write_all(&mut b, head.get_mut())?;
                for (from, to) in &items {
                    let root = src.join(from);
                    if !root.exists() {
                        continue;
                    }
                    if root.is_file() {
                        let prefix = if dest.is_empty() { to.clone() } else { format!("{dest}/{to}") };
                        let mut tw = TarWriter::new(Vec::new());
                        let parent = Path::new(&prefix).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                        for d in source::ancestors(&parent).into_iter().skip(source::ancestors(&dest).len()) {
                            tw.dir(&d, 0o755)?;
                        }
                        let mut f = std::fs::File::open(&root)?;
                        let meta = f.metadata()?;
                        use std::os::unix::fs::PermissionsExt;
                        tw.file_reader(&prefix, if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 }, meta.len(), &mut f)?;
                        std::io::Write::write_all(&mut b, tw.get_mut())?;
                        continue;
                    }
                    let prefix = if dest.is_empty() { to.clone() } else { format!("{dest}/{to}") };
                    let entries = source::walk(&root, &Ignore::new(&[]))?;
                    let mut sub_head = TarWriter::new(Vec::new());
                    let base_anc = source::ancestors(&dest).len();
                    for d in source::ancestors(&prefix).into_iter().skip(base_anc) {
                        sub_head.dir(&d, 0o755)?;
                    }
                    std::io::Write::write_all(&mut b, sub_head.get_mut())?;
                    source::stream_tree_into(&root, &entries, &prefix, false, &mut b)?;
                }
                b.finish()
            })
            .await?
        }
        LayerFrom::WorkFile { path, mode } => {
            let file = PathBuf::from(ctx.subst(path));
            let mode = *mode;
            tokio::task::spawn_blocking(move || {
                let mut head = TarWriter::new(Vec::new());
                let parent = Path::new(&dest).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                for d in source::ancestors(&parent) {
                    head.dir(&d, 0o755)?;
                }
                let mut f = std::fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
                let size = f.metadata()?.len();
                head.file_reader(&dest, mode, size, &mut f)?;
                let frags = vec![std::mem::take(head.get_mut())];
                layer::from_fragments(&store, &comment, frags, opts)
            })
            .await?
        }
        LayerFrom::Tool { tool, files } => {
            let t = ctx.tools().into_iter().find(|t| &t.name == tool).ok_or_else(|| anyhow!("toolchain {tool} not installed"))?;
            let files = files.clone();
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(Vec::new());
                let mut dirs = std::collections::BTreeSet::new();
                for (_, to) in &files {
                    let full = if dest.is_empty() { to.clone() } else { format!("{dest}/{to}") };
                    let parent = Path::new(&full).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    for d in source::ancestors(&parent) {
                        dirs.insert(d);
                    }
                }
                for d in &dirs {
                    tw.dir(d, 0o755)?;
                }
                for (from, to) in &files {
                    let full = if dest.is_empty() { to.clone() } else { format!("{dest}/{to}") };
                    let src = t.root.join(from);
                    let mut f = std::fs::File::open(&src).with_context(|| format!("opening {}", src.display()))?;
                    let size = f.metadata()?.len();
                    tw.file_reader(&full, 0o755, size, &mut f)?;
                }
                layer::from_fragments(&store, &comment, vec![std::mem::take(tw.get_mut())], opts)
            })
            .await?
        }
        LayerFrom::Upper { step: from_step, include, exclude } => {
            let upper = match ctx.get(from_step) {
                Out::Upper(p) => p,
                _ => bail!("step {from_step} has no filesystem output"),
            };
            let include = include.clone();
            let exclude = exclude.clone();
            tokio::task::spawn_blocking(move || {
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                source::stream_upper(&upper, &dest, &include, &exclude, &mut b)?;
                b.finish()
            })
            .await?
        }
        LayerFrom::ToolTree { tool } => {
            let t = ctx.tools().into_iter().find(|t| &t.name == tool).ok_or_else(|| anyhow!("toolchain {tool} not installed"))?;
            tokio::task::spawn_blocking(move || {
                let entries = source::walk(&t.root, &Ignore::new(&[".acro-complete".into(), ".acro-std-*".into()]))?;
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                source::stream_tree_into(&t.root, &entries, &dest, true, &mut b)?;
                b.finish()
            })
            .await?
        }
        LayerFrom::Inline { files } => {
            let files = files.clone();
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(Vec::new());
                for d in source::ancestors(&dest) {
                    tw.dir(&d, 0o755)?;
                }
                for (name, content) in &files {
                    let p = if dest.is_empty() { name.clone() } else { format!("{dest}/{name}") };
                    let mode = if name.ends_with(".sh") { 0o755 } else { 0o644 };
                    tw.file_bytes(&p, mode, content.as_bytes())?;
                }
                layer::from_fragments(&store, &comment, vec![std::mem::take(tw.get_mut())], opts)
            })
            .await?
        }
        LayerFrom::Image { image, include } => {
            let (layers, _) = image_rootfs(ctx, image).await?;
            let include = include.clone();
            let image = image.clone();
            tokio::task::spawn_blocking(move || {
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                let ig = Ignore::new(&[".wh.*".into(), "**/.wh.*".into()]);
                for path in &include {
                    let rel = path.trim_matches('/');
                    let target = if dest.is_empty() { rel.to_string() } else { format!("{dest}/{rel}") };
                    let found: Vec<PathBuf> = layers.iter().map(|l| l.join(rel)).filter(|p| std::fs::symlink_metadata(p).is_ok()).collect();
                    if found.is_empty() {
                        bail!("{rel} not found in {image}");
                    }
                    let mut head = TarWriter::new(Vec::new());
                    let parent = Path::new(&target).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    for d in source::ancestors(&parent) {
                        head.dir(&d, 0o755)?;
                    }
                    std::io::Write::write_all(&mut b, head.get_mut())?;
                    for p in found {
                        let meta = std::fs::symlink_metadata(&p)?;
                        let mut tw = TarWriter::new(Vec::new());
                        if meta.file_type().is_symlink() {
                            tw.symlink(&target, &std::fs::read_link(&p)?.to_string_lossy())?;
                            std::io::Write::write_all(&mut b, tw.get_mut())?;
                        } else if meta.is_file() {
                            use std::os::unix::fs::PermissionsExt;
                            let mut f = std::fs::File::open(&p)?;
                            tw.file_reader(&target, meta.permissions().mode() & 0o7777, meta.len(), &mut f)?;
                            std::io::Write::write_all(&mut b, tw.get_mut())?;
                        } else {
                            let entries = source::walk(&p, &ig)?;
                            source::stream_tree_into(&p, &entries, &target, true, &mut b)?;
                        }
                    }
                }
                b.finish()
            })
            .await?
        }
        LayerFrom::NodeModules { .. } => {
            let state = ctx
                .dep_outputs(step)
                .into_iter()
                .find_map(|o| if let Out::Npm(s) = o { Some(s) } else { None })
                .ok_or_else(|| anyhow!("node_modules layer without fetched packages"))?;
            tokio::task::spawn_blocking(move || {
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                acro_npm::install::stream_node_modules(&state.plan, &state.tarballs, &dest, |_| true, &mut |frag| {
                    std::io::Write::write_all(&mut b, &frag)?;
                    Ok(())
                })?;
                b.finish()
            })
            .await?
        }
    }
}
