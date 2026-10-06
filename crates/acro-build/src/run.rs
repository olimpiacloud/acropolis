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
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub plan_hash: String,
    pub manifest_digest: String,
    pub reference: Option<String>,
    pub layers: Vec<Layer>,
    pub base: Option<String>,
    pub ms: u64,
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
}

struct Ctx {
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
        s.replace("{work}", &self.work.to_string_lossy())
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
    let store = Arc::new(Store::open(opts.home.join("store"))?);
    let fetcher = Fetcher::new(store.clone(), opts.concurrency)?;
    let registry = Arc::new(make_registry(fetcher.client().clone(), &opts.mirrors));
    let work = opts.home.join("work").join(format!("{}-{}", &plan.hash[..12], std::process::id()));
    std::fs::create_dir_all(&work)?;
    let src = work.join("src");
    acro_events::emit(acro_events::Event::PlanReady { hash: plan.hash.clone(), steps: plan.steps.len() });
    let ctx = Arc::new(Ctx {
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
            match run_step(&ctx2, &step2).await {
                Ok(out) => {
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
    let mut first_err: Option<Arc<anyhow::Error>> = None;
    for h in order {
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(Arc::new(anyhow!("task panicked: {e}")));
                }
            }
        }
    }
    if !opts.keep_work {
        let _ = std::fs::remove_dir_all(&work);
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
    })
}

async fn run_step(ctx: &Arc<Ctx>, step: &Step) -> Result<Out> {
    match &step.action {
        Action::ResolveBase { image } => resolve_base(ctx, step, image).await,
        Action::ResolveNodeBase { spec, variant } => {
            let version = acro_toolchain::node::resolve(&ctx.fetcher, spec).await?;
            resolve_base(ctx, step, &format!("node:{version}-{variant}")).await
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
                    let version = acro_toolchain::node::resolve(fetcher, spec).await?;
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
                    let inst = acro_toolchain::node::install(fetcher, &version, &dest, p).await?;
                    std::fs::write(&marker, "")?;
                    acro_events::log(&step.id, format!("node {version}"));
                    Ok(Out::Tool(inst))
                }
                "go" => {
                    let version = acro_toolchain::go::resolve(fetcher, spec).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("go-{version}"));
                    let marker = dest.join(".acro-complete");
                    if marker.exists() {
                        return Ok(Out::Tool(Installed {
                            name: "go".into(),
                            version,
                            bin_dir: dest.join("bin"),
                            root: dest,
                            archive: None,
                        }));
                    }
                    let inst = acro_toolchain::go::install(fetcher, &version, &dest).await?;
                    std::fs::write(&marker, "")?;
                    acro_events::log(&step.id, format!("go {version}"));
                    Ok(Out::Tool(inst))
                }
                "rust" => {
                    let release = acro_cargo::resolve(fetcher, spec, &[]).await?;
                    let dest = ctx.opts.home.join("toolchains").join(format!("rust-{}", release.version));
                    let marker = dest.join(".acro-complete");
                    if !marker.exists() {
                        acro_cargo::install(fetcher, &release, &dest).await?;
                        std::fs::write(&marker, "")?;
                    }
                    acro_events::log(&step.id, format!("rust {} ({})", release.version, release.date));
                    Ok(Out::Tool(Installed { name: "rust".into(), version: release.version, bin_dir: dest.join("bin"), root: dest, archive: None }))
                }
                other => bail!("unsupported toolchain {other}"),
            }
        }
        Action::NpmFetch { manager, lockfile, dev, .. } => {
            let opts = InstallOptions { include_dev: *dev, include_optional: true, platform: Default::default() };
            let plan = if lockfile.is_empty() {
                let pj: serde_json::Value = serde_json::from_slice(&std::fs::read(ctx.opts.app_dir.join("package.json"))?)?;
                let ws = acro_npm::yarn::expand_workspaces(&ctx.opts.app_dir, &pj);
                acro_npm::resolve::plan_without_lockfile(&ctx.fetcher, &pj, &ws, &opts).await?
            } else {
                install_plan_for(manager, &ctx.opts.app_dir, lockfile, &opts)?
            };
            for p in &plan.packages {
                if let acro_npm::Source::Git { url } = &p.source {
                    bail!("git dependency {} ({url}) is not supported yet", p.path);
                }
            }
            let tarballs = acro_npm::install::fetch_all(&ctx.fetcher, &plan).await?;
            acro_events::log(&step.id, format!("{} packages ({} tarballs)", plan.packages.len(), tarballs.len()));
            Ok(Out::Npm(Arc::new(NpmState { plan, tarballs })))
        }
        Action::CopySource { exclude } => {
            let app = ctx.opts.app_dir.clone();
            let src = ctx.src.clone();
            let ex = exclude.clone();
            let n = tokio::task::spawn_blocking(move || {
                let ig = Ignore::load(&app, &ex);
                source::copy_tree(&app, &src, &ig)
            })
            .await??;
            acro_events::log(&step.id, format!("{:.1} MB", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::NpmInstall { .. } => {
            let state = ctx
                .dep_outputs(step)
                .into_iter()
                .find_map(|o| if let Out::Npm(s) = o { Some(s) } else { None })
                .ok_or_else(|| anyhow!("install without fetched packages"))?;
            let root = ctx.src.clone();
            let n = tokio::task::spawn_blocking(move || acro_npm::install::materialize(&state.plan, &state.tarballs, &root))
                .await??;
            acro_events::log(&step.id, format!("{:.1} MB written", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::GoModules { .. } => {
            let text = std::fs::read_to_string(ctx.opts.app_dir.join("go.sum"))?;
            let sum = acro_gomod::parse_go_sum(&text)?;
            let cache = acro_gomod::ModCache { root: ctx.work.join("gomodcache") };
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
        Action::CargoVendor { .. } => {
            let text = std::fs::read_to_string(ctx.opts.app_dir.join("Cargo.lock"))?;
            let lock = acro_cargo::parse_lock(&text)?;
            let vendor_dir = ctx.work.join("vendor");
            let stats = acro_cargo::vendor(&ctx.fetcher, &lock, &vendor_dir).await?;
            let cargo_home = ctx.work.join("cargo-home");
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
            if tools.iter().any(|t| t.name == "go") {
                full_env.insert("GOPATH".into(), ctx.work.join("gopath").to_string_lossy().into_owned());
                full_env.insert("GOMODCACHE".into(), ctx.work.join("gomodcache").to_string_lossy().into_owned());
                full_env.insert("GOCACHE".into(), ctx.work.join("gocache").to_string_lossy().into_owned());
                full_env.insert("GOPROXY".into(), "off".into());
                full_env.insert("GOSUMDB".into(), "off".into());
                full_env.insert("GOTOOLCHAIN".into(), "local".into());
                full_env.insert("GOFLAGS".into(), "-mod=readonly".into());
            }
            if tools.iter().any(|t| t.name == "rust") {
                let cargo_home = ctx.work.join("cargo-home");
                std::fs::create_dir_all(&cargo_home)?;
                full_env.insert("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned());
                full_env.insert("CARGO_TARGET_DIR".into(), ctx.work.join("target").to_string_lossy().into_owned());
                full_env.insert("CARGO_TERM_COLOR".into(), "never".into());
                full_env.insert("CARGO_INCREMENTAL".into(), "0".into());
            }
            if let Some(node) = tools.iter().find(|t| t.name == "node") {
                full_env.insert("npm_node_execpath".into(), node.bin_dir.join("node").to_string_lossy().into_owned());
                full_env.insert("INIT_CWD".into(), cwd_path.to_string_lossy().into_owned());
            }
            for (k, v) in env {
                full_env.insert(k.clone(), ctx.subst(v));
            }
            let argv: Vec<String> = argv.iter().map(|a| ctx.subst(a)).collect();
            let goflags_vendor = argv.iter().any(|a| a == "-mod=vendor");
            if goflags_vendor {
                full_env.remove("GOFLAGS");
            }
            ctx.exec
                .run(Cmd { step: step.id.clone(), argv, cwd: cwd_path, env: full_env, network: *network })
                .await?;
            Ok(Out::None)
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
                env: spec.env.clone(),
                entrypoint: spec.entrypoint.clone(),
                cmd: spec.cmd.clone(),
                workdir: spec.workdir.clone(),
                user: spec.user.clone(),
                exposed_ports: spec.ports.clone(),
                labels,
            };
            let assembled = assemble::assemble(base.as_deref(), &layers, &patch)?;
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

async fn resolve_base(ctx: &Arc<Ctx>, step: &Step, image: &str) -> Result<Out> {
    let r = Reference::parse(image)?;
    let (reference, manifest, digest) = ctx.registry.resolve_manifest(&r, &ctx.opts.platform).await?;
    if let Some(target) = ctx.opts.target.clone() {
        let reg = ctx.registry.clone();
        let src = reference.clone();
        let layers = manifest.layers.clone();
        let handle = tokio::spawn(async move { assemble::copy_layers(&reg, &src, &layers, &target).await });
        *ctx.base_copy.lock().unwrap() = Some(handle);
    }
    let resolved = ctx.registry.with_config(reference, manifest, digest).await?;
    acro_events::log(&step.id, format!("{} -> {}", image, resolved.manifest_digest));
    Ok(Out::Base(Arc::new(resolved)))
}

async fn build_layer(ctx: &Arc<Ctx>, step: &Step, dest: &str, from: &LayerFrom) -> Result<Layer> {
    let opts = ctx.opts.layer;
    let store = ctx.store.clone();
    let comment = step.id.clone();
    let dest = dest.trim_matches('/').to_string();
    match from {
        LayerFrom::AppSource { exclude } => {
            let app = ctx.opts.app_dir.clone();
            let ex = exclude.clone();
            tokio::task::spawn_blocking(move || {
                let ig = Ignore::load(&app, &ex);
                source::dir_layer(&store, &app, &ig, &dest, &comment, opts)
            })
            .await?
        }
        LayerFrom::WorkDir { path, exclude } => {
            let root = if path == "." { ctx.src.clone() } else { ctx.src.join(path) };
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
                let mut frags = Vec::new();
                let mut head = TarWriter::new(Vec::new());
                for d in source::ancestors(&dest) {
                    head.dir(&d, 0o755)?;
                }
                frags.push(std::mem::take(head.get_mut()));
                for (from, to) in &items {
                    let root = src.join(from);
                    if !root.exists() {
                        continue;
                    }
                    let prefix = if dest.is_empty() { to.clone() } else { format!("{dest}/{to}") };
                    let entries = source::walk(&root, &Ignore::new(&[]))?;
                    let mut sub_head = TarWriter::new(Vec::new());
                    let base_anc = source::ancestors(&dest).len();
                    for d in source::ancestors(&prefix).into_iter().skip(base_anc) {
                        sub_head.dir(&d, 0o755)?;
                    }
                    frags.push(std::mem::take(sub_head.get_mut()));
                    frags.extend(source::fragments_for(&root, &entries, &prefix, false)?);
                }
                layer::from_fragments(&store, &comment, frags, opts)
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
        LayerFrom::Inline { files } => {
            let files = files.clone();
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(Vec::new());
                for d in source::ancestors(&dest) {
                    tw.dir(&d, 0o755)?;
                }
                for (name, content) in &files {
                    let p = if dest.is_empty() { name.clone() } else { format!("{dest}/{name}") };
                    tw.file_bytes(&p, 0o644, content.as_bytes())?;
                }
                layer::from_fragments(&store, &comment, vec![std::mem::take(tw.get_mut())], opts)
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
                let frags = acro_npm::install::node_modules_fragments(&state.plan, &state.tarballs, &dest, |_| true)?;
                layer::from_fragments(&store, &comment, frags.fragments, opts)
            })
            .await?
        }
    }
}
