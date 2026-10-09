use crate::detect::Env;
use crate::ignore::Ignore;
use crate::plan::{Action, LayerFrom, Plan, Step};
use crate::source;
use acropolis_exec::{Cmd, Executor};
use acropolis_fetch::Fetcher;
use acropolis_npm::{InstallOptions, InstallPlan, PackageLock, Tarballs};
use acropolis_oci::assemble::{self, ConfigPatch};
use acropolis_oci::image::Platform;
use acropolis_oci::layer::{self, Layer, LayerOptions};
use acropolis_oci::tar::TarWriter;
use acropolis_oci::{Reference, Registry, ResolvedImage};
use acropolis_store::Store;
use acropolis_toolchain::Installed;
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
        acropolis_store::sha256_bytes(canon.to_string_lossy().as_bytes()).hex()[..16].to_string()
    });
    let dir = crate::cache::app_dir(&opts.home, &key);
    std::fs::create_dir_all(&dir).ok()?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .ok()?;
    let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        acropolis_events::log(
            "cache",
            format!("app cache {key} is in use by another build; building without it"),
        );
        return None;
    }
    crate::gc::touch(&dir.join(".lock"));
    Some(AppCache { dir, _lock: lock })
}

/// sha256 of `packages/yarnpkg-cli/bin/yarn.js` for every yarn 2.x tag on repo.yarnpkg.com
/// (2.x has no checksummed source; 2.4.3 is not on npm). Each matched the file at the
/// `@yarnpkg/cli/<version>` tag on GitHub; 2.4.1 and 2.4.2 also match npm's `@yarnpkg/cli-dist`.
const YARN2_SHA256: &[(&str, &str)] = &[
    (
        "2.4.3",
        "8c1575156cfa42112242cc5cfbbd1049da9448ffcdb5c55ce996883610ea983f",
    ),
    (
        "2.4.2",
        "3ceae9c55cc9f52b474922a5c9bdfb32ada8fa003a3e071064e157fc156cfad0",
    ),
    (
        "2.4.1",
        "8feb0398db243b0f9deaece6d71b5d18e0dc8795758d41cbe9d1efc70038865e",
    ),
    (
        "2.4.0",
        "20e1fc90678be9e6fa93534af62b115f84d3c54f619cd654af06dabd298e2b5e",
    ),
    (
        "2.3.3",
        "63ea33a54bcabe4497fb3c5b2e2131777e1be68afb555356b2756257d9d05141",
    ),
    (
        "2.3.2",
        "875359e6a2495b7fd7833b9df53b9254448e9f40903349c11a8a99cd5b8c5d19",
    ),
    (
        "2.3.1",
        "545fa35150042a6ff59476170b0e52e589ed17744897c7f824768dec4ad09568",
    ),
    (
        "2.3.0",
        "8f8fc1d45edd1fb75eb8fc82dd795c26a844d24984a793799c161f0d6810ca02",
    ),
    (
        "2.2.2",
        "7053adad688cd251fa2aa0153435029ec12df9f4c133032bfd4fd3914c9b7c4e",
    ),
    (
        "2.2.1",
        "bd281825e52cb56177d54465e100049d1a8a95a670e0422fa6cc22ca09e54fd7",
    ),
    (
        "2.2.0",
        "c70b37c341dd281237cd4dd53b4bd7c771f79d644023562f1e4785efb9aad154",
    ),
    (
        "2.1.1",
        "df639efa2d01320b42ae58c59aba04556a17f54680e8478351e51eb61448c1b2",
    ),
    (
        "2.1.0",
        "afd7b34d2f0c13b5aba0cca8ff38bbfcaa789da6c1151c7ced6c9a7211ed7814",
    ),
    (
        "2.0.0-rc.36",
        "e64ccc6e7147784c8b2be90683ef9d5b2b44e18828190706e45b53ab5fcd68ae",
    ),
    (
        "2.0.0-rc.35",
        "cb21818590bf5559627d14748a8d68c789adb624fe7477e42f565c9acc7f9afe",
    ),
    (
        "2.0.0-rc.34",
        "e3f245ee13d70e70f0139007cbb385ba52d7069a5f63e18a36f43b40291a7917",
    ),
    (
        "2.0.0-rc.33",
        "a00b561a5036d32842bb06630382bb6d517189f97c461a4905da21ea0fcd14c4",
    ),
    (
        "2.0.0-rc.32",
        "75f0407cfe9925d2f67fdcf14ca6c9541296192e2afe47f8e0efdf6b5f9251e2",
    ),
    (
        "2.0.0-rc.31",
        "0391ce89ea589aeae5c5de9ead85a4239de2fbd67589752e74f762f391f19446",
    ),
    (
        "2.0.0-rc.30",
        "19ce531b56d31f4e29aa12d0d69a823b032298cb556c6b2d4eab5a793bdb3926",
    ),
    (
        "2.0.0-rc.29",
        "0b948cb656dce58595c96c8eece7fa43d9ddb9fa789c8af811db9724797f99b6",
    ),
    (
        "2.0.0-rc.28",
        "bd22de0cc9898c5c9457f09127f4b91ea8b3eb9eaba697e33788a75954777d45",
    ),
    (
        "2.0.0-rc.27",
        "24af7451bac0da443ae6894b53df88bc4aaa6967d5f52786f0c9870f1e13338c",
    ),
    (
        "2.0.0-rc.26",
        "dbfcc081e606a045e68bf8c7f9f0c90871a23361fc24ef469046d9cd5ecab68e",
    ),
    (
        "2.0.0-rc.25",
        "971da0553ed69f9374b264bf4c9aa634b5652009088f4ee970421a9f75c1fc4d",
    ),
    (
        "2.0.0-rc.24",
        "1880659563630bdd4d060ab352dda54ba8f94dfdf951fabfbbae75f339416d50",
    ),
    (
        "2.0.0-rc.23",
        "f5fd88f6b241689a913e488cc4909e0a99cb4fd7bfa9b7407fad473c1bedc7d8",
    ),
    (
        "2.0.0-rc.22",
        "9d5311d6fc05c2c99ec45c7e5b4a494ce1870cf3914081bb762c6d11778157b7",
    ),
    (
        "2.0.0-rc.21",
        "e2301f32270f92c4ffe3059fefc47e7732a66ea3387c1a14995c85a8ec769342",
    ),
    (
        "2.0.0-rc.20",
        "7d664a6e56e6619bfc63c572a26a2565cdf5a54b5c84928397e8a8860f4d76dd",
    ),
    (
        "2.0.0-rc.19",
        "eac662a793f19a7167edd1d176f480304a96afbf2e4c1399377fbd7386f8d56d",
    ),
    (
        "2.0.0-rc.18",
        "1f2c614d54b6383d092e15c8ae52f6b73d60dad35f561a301e3b1188b4b6f820",
    ),
    (
        "2.0.0-rc.17",
        "2871dbfab49ffa53d11fbdebcb6514bcd883fef78b55298e7cf8381db23a47d5",
    ),
    (
        "2.0.0-rc.16",
        "fd71ed1d51350a3d6e3d8fad732d8e4d1e6ab70520dec4785941fad6b98ec98e",
    ),
    (
        "2.0.0-rc.15",
        "84c4a2c8da9eb61915a93d3392aaa0c37bd6eb3a31f0f6638e298ad8d078d74d",
    ),
    (
        "2.0.0-rc.14",
        "99a6d335e070598f92cc4e58c3da551cab2bf8315ebc9d05a6ca3432059f3fae",
    ),
    (
        "2.0.0-rc.13",
        "cca14d4965c8e20efb4767c662d7aeecc8692c8a758027fdff8fa7736552b675",
    ),
    (
        "2.0.0-rc.12",
        "a3f296039b711dac6295c5d45f57baf5c93a8932c537a608561b9b2319e0b973",
    ),
    (
        "2.0.0-rc.11",
        "cdba716fd643f5ab354c3f4f058d6af02b08897aa53bd9516739163e2d22f090",
    ),
    (
        "2.0.0-rc.10",
        "fbe28d52c2634abe8248767ca636fe821a388d6e6f9e6464cc57a6bef0075657",
    ),
    (
        "2.0.0-rc.9",
        "fe63f6cf9b5b0c34489946a342fd0b94fd6a864e75c3889e97e3de74acca698e",
    ),
    (
        "2.0.0-rc.8",
        "5b459439227b791ba4d08cfc88c353f172d833c29ab3ea1f7bf244db3b170d83",
    ),
    (
        "2.0.0-rc.7",
        "8672c296fa377a067102de475634f4586647d855d5a397ab1885791fef80926c",
    ),
    (
        "2.0.0-rc.6",
        "97c819d4a630c4cc2efe6c59bb486cb70e628470c6a780c7d520381c7428b75f",
    ),
    (
        "2.0.0-rc.5",
        "da0e2e98c02427703bedf984151847a5647718176ab1b436d8f07166f0d7764a",
    ),
    (
        "2.0.0-rc.4",
        "6935ad0cdf0bfc1e6ea5fe9ec8a1338139026db9d6050e5554b7747b021f284f",
    ),
];

const NODE_CACHE_DIRS: &[&str] = &[".next/cache", "node_modules/.cache"];

fn restore_node_caches(cache: &Path, cwd: &Path) {
    for rel in NODE_CACHE_DIRS {
        let saved = cache.join("node").join(rel.replace('/', "__"));
        let live = cwd.join(rel);
        if saved.is_dir() && !symlink_on_the_way(cwd, rel) && std::fs::symlink_metadata(&live).is_err() {
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
        if !symlink_on_the_way(cwd, rel) && std::fs::symlink_metadata(&live).is_ok_and(|m| m.is_dir()) {
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
            let r = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
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
    let Ok(key) = std::fs::read_to_string(cache.join("nm.key")) else {
        return;
    };
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
    let Ok(list) = std::fs::read_to_string(prev.join(".list")) else {
        return false;
    };
    for line in list.lines() {
        let Some((i, rel)) = line.split_once('\t') else {
            continue;
        };
        if clean_relative(rel).is_err() || symlink_on_the_way(src, rel) {
            return false;
        }
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
    late: Mutex<HashMap<String, Vec<StepFuture>>>,
}

type StepFuture = Shared<BoxFuture<'static, Result<(), Arc<anyhow::Error>>>>;

/// A step error shared between the futures that await it. Keeps the original
/// chain so `errors::classify` still finds the typed `CommandFailed`.
#[derive(Debug)]
struct StepError(Arc<anyhow::Error>);

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self.0, f)
    }
}

impl std::error::Error for StepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl Ctx {
    fn cache_path(&self, kind: &str) -> PathBuf {
        match &self.cache {
            Some(c) => c.dir.join(kind),
            None => self.work.join(kind),
        }
    }

    /// What a host step may write: this build's work dir and this app's cache. The rest of the
    /// builder (the app dir, other apps' caches, the store, /usr, /etc) is read-only for it.
    fn step_writable(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.work.clone()];
        if let Some(c) = &self.cache {
            dirs.push(c.dir.clone());
        }
        dirs
    }

    fn get(&self, id: &str) -> Out {
        self.out.lock().unwrap().get(id).cloned().unwrap_or(Out::None)
    }

    async fn await_late(&self, step: &Step) -> Result<()> {
        let late = self.late.lock().unwrap().remove(&step.id).unwrap_or_default();
        for f in late {
            f.await.map_err(|e| anyhow::Error::new(StepError(e)))?;
        }
        Ok(())
    }

    fn dep_outputs(&self, step: &Step) -> Vec<Out> {
        step.deps.iter().map(|d| self.get(d)).collect()
    }

    fn base(&self) -> Option<Arc<ResolvedImage>> {
        self.out
            .lock()
            .unwrap()
            .values()
            .find_map(|o| if let Out::Base(b) = o { Some(b.clone()) } else { None })
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
            let value = if crate::detect::operator_key(&name) {
                String::new()
            } else {
                self.opts.env.vars.get(&name).cloned().unwrap_or_default()
            };
            out.replace_range(start..start + end + 1, &value);
            from = start + value.len();
        }
        while let Some(start) = out.find("{tool:") {
            let Some(end) = out[start..].find('}') else { break };
            let name = out[start + 6..start + end].to_string();
            let root = self
                .tools()
                .into_iter()
                .find(|t| t.name == name)
                .map(|t| t.root.to_string_lossy().into_owned())
                .unwrap_or_default();
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
            let v = self
                .tools()
                .into_iter()
                .find(|t| t.name == name)
                .map(|t| t.version)
                .unwrap_or_else(|| fallback.to_string());
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
    let work = opts
        .home
        .join("work")
        .join(format!("{}-{}", &plan.hash[..12], std::process::id()));
    std::fs::create_dir_all(&work)?;

    acropolis_events::emit(acropolis_events::Event::PlanReady {
        hash: plan.hash.clone(),
        steps: plan.steps.len(),
    });
    let home_lock = crate::gc::shared(&opts.home);
    let cache = open_app_cache(&opts);
    let stable_src = opts.env.config("CACHE_SRC").map(|(v, _)| v != "0").unwrap_or(true);
    let src = match cache.as_ref().filter(|_| stable_src) {
        Some(c) => {
            acropolis_events::log("cache", format!("using app cache {}", c.dir.display()));
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
                    let mine = name.ends_with(&format!("-{}", std::process::id()))
                        || name.contains(&format!("-{}-", std::process::id()));
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
        late: Mutex::new(HashMap::new()),
    });
    let secs = |name: &str| {
        opts.env
            .config(name)
            .and_then(|(v, _)| v.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(std::time::Duration::from_secs)
    };
    let step_timeout = secs("STEP_TIMEOUT");
    let build_timeout = match opts.env.config("BUILD_TIMEOUT").map(|(v, _)| v) {
        Some(v) if v.trim() == "0" => None,
        _ => secs("BUILD_TIMEOUT").or(Some(std::time::Duration::from_secs(3600))),
    };
    let mut futs: HashMap<String, StepFuture> = HashMap::new();
    let mut order = Vec::new();
    for step in &plan.steps {
        let late_ids: Vec<&String> = match &step.action {
            Action::NpmInstall { .. } => step
                .deps
                .iter()
                .filter(|d| {
                    plan.step(d)
                        .is_some_and(|s| matches!(s.action, Action::Toolchain { .. }))
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut deps: Vec<StepFuture> = Vec::new();
        let mut late: Vec<StepFuture> = Vec::new();
        for d in &step.deps {
            let f = futs
                .get(d)
                .cloned()
                .ok_or_else(|| anyhow!("step {} depends on unknown {}", step.id, d))?;
            if late_ids.contains(&d) {
                late.push(f)
            } else {
                deps.push(f)
            }
        }
        if !late.is_empty() {
            ctx.late.lock().unwrap().insert(step.id.clone(), late);
        }
        let ctx2 = ctx.clone();
        let step2 = step.clone();
        let fut = async move {
            for d in deps {
                d.await?;
            }
            let guard = acropolis_events::step(&step2.id, &step2.name);
            let touch_tool = |out: &Out| {
                if let Out::Tool(t) = out {
                    crate::gc::touch(&t.root.join(".acropolis-complete"));
                }
            };
            let res = match step_timeout {
                Some(t) => match tokio::time::timeout(t, run_step(&ctx2, &step2)).await {
                    Ok(r) => r,
                    Err(_) => Err(anyhow!("timed out after {}s (ACROPOLIS_STEP_TIMEOUT)", t.as_secs())),
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
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
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
            _ = &mut deadline, if first_err.is_none() => Some(format!("build timed out after {}s (ACROPOLIS_BUILD_TIMEOUT)", build_timeout.map(|t| t.as_secs()).unwrap_or(0))),
            _ = sigint.recv(), if first_err.is_none() => Some("build interrupted (SIGINT)".to_string()),
            _ = sigterm.recv(), if first_err.is_none() => Some("build interrupted (SIGTERM)".to_string()),
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
    if let Some(max) = opts
        .env
        .config("CACHE_MAX")
        .and_then(|(v, _)| crate::gc::parse_size(&v))
    {
        let rootfs = std::env::var_os("ACROPOLIS_ROOTFS")
            .map(PathBuf::from)
            .unwrap_or_else(|| opts.home.join("rootfs"));
        let home = opts.home.clone();
        let _ = tokio::task::spawn_blocking(move || crate::gc::collect(&home, &rootfs, max)).await;
    }
    if let Some(e) = first_err {
        return Err(anyhow::Error::new(StepError(e)));
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
        Action::ResolveNodeBase { spec, variant } if variant.starts_with(crate::providers::node::DISTROLESS) => {
            let version =
                acropolis_toolchain::node::resolve(&ctx.fetcher, &acropolis_semver::fuzzy_version(spec)).await?;
            let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
            if crate::providers::node::DISTROLESS_NODE_MAJORS.contains(&major) {
                let order = if variant.ends_with("debian13") {
                    ["debian13", "debian12"]
                } else {
                    ["debian12", "debian13"]
                };
                for debian in order {
                    let image = format!("gcr.io/distroless/nodejs{major}-{debian}:debug");
                    match resolve_base(ctx, step, &image).await {
                        Ok(out) => return Ok(out),
                        Err(e) if missing_manifest(&e) => continue,
                        Err(e) => return Err(e),
                    }
                }
            }
            acropolis_events::log(
                &step.id,
                format!("no distroless image for node {version}: using node:{version}-bookworm-slim"),
            );
            resolve_base(ctx, step, &format!("node:{version}-bookworm-slim")).await
        }
        Action::ResolveNodeBase { spec, variant } => {
            let version = acropolis_toolchain::node::resolve(&ctx.fetcher, spec).await?;
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
            acropolis_events::log(&step.id, format!("copied {:.1} MB", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::Toolchain { tool, spec, parts } => {
            let fetcher = &ctx.fetcher;
            match tool.as_str() {
                "node" => {
                    let fuzzy = acropolis_semver::fuzzy_version(spec);
                    let version = acropolis_toolchain::node::resolve(fetcher, &fuzzy).await?;
                    let dest = toolchain_dir(&ctx.opts.home, &format!("node-{version}"))?;
                    let marker = dest.join(".acropolis-complete");
                    if marker.exists() {
                        return Ok(Out::Tool(Installed {
                            name: "node".into(),
                            version,
                            bin_dir: dest.join("bin"),
                            root: dest,
                            archive: None,
                        }));
                    }
                    let p = acropolis_toolchain::node::Parts {
                        npm: parts.iter().any(|p| p == "npm"),
                        corepack: parts.iter().any(|p| p == "corepack"),
                        headers: parts.iter().any(|p| p == "headers"),
                    };
                    let tmp = staging(&dest);
                    acropolis_toolchain::node::install(fetcher, &version, &tmp, p).await?;
                    publish(&tmp, &dest)?;
                    acropolis_events::log(&step.id, format!("node {version}"));
                    Ok(Out::Tool(Installed {
                        name: "node".into(),
                        version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "go" => {
                    let version = acropolis_toolchain::go::resolve(fetcher, spec).await?;
                    let dest = toolchain_dir(&ctx.opts.home, &format!("go-{version}"))?;
                    let marker = dest.join(".acropolis-complete");
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
                    acropolis_toolchain::go::install(fetcher, &version, &tmp).await?;
                    publish(&tmp, &dest)?;
                    acropolis_events::log(&step.id, format!("go {version}"));
                    if ctx.opts.env.flag("PREWARM") {
                        precompile_go_std(ctx, step, &dest).await?;
                    }
                    Ok(Out::Tool(Installed {
                        name: "go".into(),
                        version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "bun" => {
                    let release = acropolis_toolchain::bun::resolve(fetcher, spec).await?;
                    let dest = toolchain_dir(&ctx.opts.home, &format!("bun-{}", release.version))?;
                    let marker = dest.join(".acropolis-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acropolis_toolchain::bun::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acropolis_events::log(&step.id, format!("bun {}", release.version));
                    Ok(Out::Tool(Installed {
                        name: "bun".into(),
                        version: release.version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "yarn-berry" => {
                    let dest = toolchain_dir(&ctx.opts.home, &format!("yarn-berry-{spec}"))?;
                    if !dest.join(".acropolis-complete").exists() {
                        let tmp = staging(&dest);
                        let Some((_, sha256)) = YARN2_SHA256.iter().find(|(v, _)| *v == spec.as_str()) else {
                            bail!(
                                "invalid toolchain version yarn {spec}: not a released yarn 2.x (set yarnPath in .yarnrc.yml or use yarn 3+)"
                            );
                        };
                        let url = format!("https://repo.yarnpkg.com/{spec}/packages/yarnpkg-cli/bin/yarn.js");
                        let expected = acropolis_store::Integrity::parse_oci(&format!("sha256:{sha256}"))?;
                        let blob = fetcher.blob("yarn.js", &url, Some(expected)).await?;
                        std::fs::create_dir_all(&tmp)?;
                        std::fs::copy(&blob.path, tmp.join("yarn.js"))?;
                        publish(&tmp, &dest)?;
                    }
                    Ok(Out::Tool(Installed {
                        name: "yarn-berry".into(),
                        version: spec.clone(),
                        bin_dir: dest.clone(),
                        root: dest,
                        archive: None,
                    }))
                }
                "composer" => {
                    let dest = ctx.opts.home.join("toolchains").join("composer-latest-stable");
                    if !dest.join(".acropolis-complete").exists() {
                        let tmp = staging(&dest);
                        let sum = fetcher
                            .bytes("https://getcomposer.org/download/latest-stable/composer.phar.sha256sum")
                            .await?;
                        let hex = String::from_utf8_lossy(&sum)
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let expected = acropolis_store::Integrity::parse_hex(acropolis_store::Algo::Sha256, &hex)?;
                        let blob = fetcher
                            .blob(
                                "composer.phar",
                                "https://getcomposer.org/download/latest-stable/composer.phar",
                                Some(expected),
                            )
                            .await?;
                        std::fs::create_dir_all(tmp.join("bin"))?;
                        std::fs::copy(&blob.path, tmp.join("bin/composer"))?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(tmp.join("bin/composer"), std::fs::Permissions::from_mode(0o755))?;
                        publish(&tmp, &dest)?;
                    }
                    Ok(Out::Tool(Installed {
                        name: "composer".into(),
                        version: "latest-stable".into(),
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "mise" => {
                    let tag = if spec.is_empty() || spec == "latest" {
                        github_latest_tag("jdx/mise").await?
                    } else {
                        format!("v{}", spec.trim_start_matches('v'))
                    };
                    let dest = toolchain_dir(&ctx.opts.home, &format!("mise-{tag}"))?;
                    if !dest.join(".acropolis-complete").exists() {
                        let tmp = staging(&dest);
                        let arch = if std::env::consts::ARCH == "aarch64" {
                            "arm64"
                        } else {
                            "x64"
                        };
                        let file = format!("mise-{tag}-linux-{arch}-musl");
                        let base = format!("https://github.com/jdx/mise/releases/download/{tag}");
                        let sums = String::from_utf8_lossy(&fetcher.bytes(&format!("{base}/SHASUMS256.txt")).await?)
                            .into_owned();
                        let expected = acropolis_toolchain::parse_shasums(&sums.replace("./", ""), &file)?;
                        let blob = fetcher.blob(&file, &format!("{base}/{file}"), Some(expected)).await?;
                        std::fs::create_dir_all(tmp.join("bin"))?;
                        std::fs::copy(&blob.path, tmp.join("bin/mise"))?;
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(tmp.join("bin/mise"), std::fs::Permissions::from_mode(0o755))?;
                        publish(&tmp, &dest)?;
                    }
                    acropolis_events::log(&step.id, format!("mise {tag}"));
                    Ok(Out::Tool(Installed {
                        name: "mise".into(),
                        version: tag,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                t if t.starts_with("npm:") => {
                    let pkg = &t[4..];
                    let release = acropolis_toolchain::npmpkg::resolve(fetcher, pkg, spec).await?;
                    let dest = toolchain_dir(
                        &ctx.opts.home,
                        &format!("npm-{}-{}", pkg.replace('/', "+"), release.version),
                    )?;
                    if !dest.join(".acropolis-complete").exists() {
                        let tmp = staging(&dest);
                        acropolis_toolchain::npmpkg::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acropolis_events::log(&step.id, format!("{pkg} {}", release.version));
                    Ok(Out::Tool(Installed {
                        name: t.to_string(),
                        version: release.version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "uv" => {
                    let release = acropolis_toolchain::uv::resolve(fetcher, spec).await?;
                    let dest = toolchain_dir(&ctx.opts.home, &format!("uv-{}", release.version))?;
                    let marker = dest.join(".acropolis-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acropolis_toolchain::uv::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acropolis_events::log(&step.id, format!("uv {}", release.version));
                    Ok(Out::Tool(Installed {
                        name: "uv".into(),
                        version: release.version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "python-standalone" => {
                    let key = if spec.is_empty() {
                        "latest".to_string()
                    } else {
                        spec.clone()
                    };
                    let dest = toolchain_dir(&ctx.opts.home, &format!("python-standalone-{key}"))?;
                    if !dest.join(".acropolis-complete").exists() {
                        let uv = acropolis_toolchain::uv::resolve(fetcher, "").await?;
                        let uv_dest = toolchain_dir(&ctx.opts.home, &format!("uv-{}", uv.version))?;
                        if !uv_dest.join(".acropolis-complete").exists() {
                            let tmp = staging(&uv_dest);
                            acropolis_toolchain::uv::install(fetcher, &uv, &tmp).await?;
                            publish(&tmp, &uv_dest)?;
                        }
                        let tmp = staging(&dest);
                        let install_dir = PathBuf::from(format!("{}.uv", tmp.display()));
                        let _ = std::fs::remove_dir_all(&install_dir);
                        let mut cmd = tokio::process::Command::new(uv_dest.join("bin/uv"));
                        cmd.arg("python").arg("install").arg("--install-dir").arg(&install_dir);
                        if !spec.is_empty() {
                            cmd.arg("--").arg(spec);
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
                                    && p.file_name()
                                        .map(|n| n.to_string_lossy().starts_with("cpython-"))
                                        .unwrap_or(false)
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
                    let version = tokio::process::Command::new(dest.join("bin/python3"))
                        .arg("-c")
                        .arg("import platform; print(platform.python_version())")
                        .output()
                        .await
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .filter(|v| !v.is_empty())
                        .unwrap_or(key);
                    acropolis_events::log(&step.id, format!("python {version} (standalone)"));
                    Ok(Out::Tool(Installed {
                        name: "python-standalone".into(),
                        version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                "rust" => {
                    let mut release = acropolis_cargo::resolve(fetcher, spec, &[]).await?;
                    if parts.iter().any(|p| p == "raise-to-crates") {
                        let text = String::from_utf8(read_app_file(&ctx.opts.app_dir, "Cargo.lock")?)
                            .context("Cargo.lock is not UTF-8")?;
                        let lock = acropolis_cargo::parse_lock(&text)?;
                        let vendor_dir = ctx.cache_path("cargo-vendor");
                        let need =
                            tokio::task::spawn_blocking(move || acropolis_cargo::max_rust_version(&lock, &vendor_dir))
                                .await?;
                        let have = acropolis_cargo::parse_rust_version(&release.version);
                        if let Some((a, b, c)) = need
                            && have.is_some_and(|h| h < (a, b, c))
                        {
                            acropolis_events::log(
                                &step.id,
                                format!("the locked crates need rustc {a}.{b}.{c}: using it instead of {spec}"),
                            );
                            release = acropolis_cargo::resolve(fetcher, &format!("{a}.{b}.{c}"), &[]).await?;
                        }
                    }
                    let dest = toolchain_dir(&ctx.opts.home, &format!("rust-{}", release.version))?;
                    let marker = dest.join(".acropolis-complete");
                    if !marker.exists() {
                        let tmp = staging(&dest);
                        acropolis_cargo::install(fetcher, &release, &tmp).await?;
                        publish(&tmp, &dest)?;
                    }
                    acropolis_events::log(&step.id, format!("rust {} ({})", release.version, release.date));
                    Ok(Out::Tool(Installed {
                        name: "rust".into(),
                        version: release.version,
                        bin_dir: dest.join("bin"),
                        root: dest,
                        archive: None,
                    }))
                }
                other => bail!("unsupported toolchain {other}"),
            }
        }
        Action::NpmFetch {
            manager,
            lockfile,
            dev,
            workspaces,
            keep,
            ..
        } => {
            let opts = InstallOptions {
                include_dev: *dev,
                include_optional: true,
                platform: Default::default(),
            };
            let locked = if lockfile.is_empty() {
                None
            } else {
                let (manager, app, lockfile) = (manager.clone(), ctx.opts.app_dir.clone(), lockfile.clone());
                let (opts, workspaces, keep) = (opts.clone(), workspaces.clone(), keep.clone());
                Some(
                    tokio::task::spawn_blocking(move || {
                        locked_plan(&manager, &app, &lockfile, &opts, &workspaces, &keep)
                    })
                    .await??,
                )
            };
            let plan = match locked {
                Some(Some(plan)) => plan,
                out_of_sync => {
                    if out_of_sync.is_some() {
                        if ctx.opts.env.flag("STRICT_LOCKFILE") {
                            bail!(
                                "package-lock.json is out of sync with package.json (ACROPOLIS_STRICT_LOCKFILE=1); run `npm install` and commit the lockfile"
                            );
                        }
                        acropolis_events::log(
                            &step.id,
                            "warning: package-lock.json is out of sync with package.json; resolving from the registry like `npm install`",
                        );
                    }
                    let pj = crate::detect::read_package_json(&ctx.opts.app_dir)?;
                    let ws = acropolis_npm::yarn::expand_workspaces(&ctx.opts.app_dir, &pj);
                    acropolis_npm::resolve::plan_without_lockfile(&ctx.fetcher, &pj, &ws, &opts).await?
                }
            };
            check_package_urls(&plan, &ctx.opts.env)?;
            for p in &plan.packages {
                if let acropolis_npm::Source::Git { url } = &p.source {
                    acropolis_events::log(
                        &step.id,
                        format!(
                            "warning: {} comes from git ({url}); it is pinned by commit, not by content hash",
                            p.path
                        ),
                    );
                }
            }
            let tarballs = acropolis_npm::install::fetch_all(&ctx.fetcher, &plan).await?;
            acropolis_events::log(
                &step.id,
                format!("{} packages ({} tarballs)", plan.packages.len(), tarballs.len()),
            );
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
            acropolis_events::log(&step.id, format!("{:.1} MB", n as f64 / 1e6));
            Ok(Out::None)
        }
        Action::NpmInstall {
            target,
            scripts,
            manager,
            types_only,
            ..
        } => {
            let state = ctx
                .dep_outputs(step)
                .into_iter()
                .find_map(|o| if let Out::Npm(s) = o { Some(s) } else { None })
                .ok_or_else(|| anyhow!("install without fetched packages"))?;
            let root = if target == "src" {
                ctx.src.clone()
            } else {
                ctx.work.join(target)
            };
            std::fs::create_dir_all(&root)?;
            let reuse = ctx
                .cache
                .as_ref()
                .filter(|c| target == "src" && ctx.src.starts_with(&c.dir))
                .map(|c| c.dir.clone());
            if reuse.is_some() {
                ctx.await_late(step).await?;
            }
            let mut versions: Vec<String> = ctx
                .dep_outputs(step)
                .into_iter()
                .filter_map(|o| {
                    if let Out::Tool(t) = o {
                        Some(format!("{}={}", t.name, t.version))
                    } else {
                        None
                    }
                })
                .collect();
            versions.sort();
            let key = format!("{} {}", step.hash, versions.join(" "));
            if let Some(cache_dir) = &reuse {
                if restore_node_modules(cache_dir, &root, &key) {
                    std::fs::write(cache_dir.join("nm.key"), &key)?;
                    acropolis_events::log(
                        &step.id,
                        "reused node_modules from the previous build (same lockfile and toolchains)",
                    );
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
            if target != "src"
                && let Ok(pj) = read_app_file(&ctx.opts.app_dir, "package.json")
            {
                std::fs::write(root.join("package.json"), pj)?;
            }
            let r2 = root.clone();
            let s2 = state.clone();
            let types = *types_only;
            let n = tokio::task::spawn_blocking(move || {
                acropolis_npm::install::materialize_with(&s2.plan, &s2.tarballs, &r2, types)
            })
            .await??;
            acropolis_events::log(&step.id, format!("{:.1} MB written", n as f64 / 1e6));
            for line in apply_patched_dependencies(&ctx.opts.app_dir, &state.plan, &root)? {
                acropolis_events::log(&step.id, line);
            }
            if manager == "pnpm"
                && !symlink_on_the_way(&root, "node_modules/.modules.yaml")
                && !root.join("node_modules/.modules.yaml").exists()
            {
                std::fs::create_dir_all(root.join("node_modules"))?;
                std::fs::write(
                    root.join("node_modules/.modules.yaml"),
                    "hoistPattern:\n  - '*'\nhoistedDependencies: {}\nincluded:\n  dependencies: true\n  devDependencies: true\n  optionalDependencies: true\nlayoutVersion: 5\nnodeLinker: isolated\npendingBuilds: []\npublicHoistPattern: []\nregistries:\n  default: https://registry.npmjs.org/\nskipped: []\nvirtualStoreDir: .pnpm\n",
                )?;
            }
            ctx.await_late(step).await?;
            if !scripts.is_empty() && scripts != "none" {
                let policy = acropolis_npm::scripts::Policy::parse(scripts);
                let mut jobs = acropolis_npm::scripts::lifecycle_jobs(&state.plan, &root, &policy);
                if target == "prod"
                    && ctx.opts.env.flag("NODE_PLAYWRIGHT_INSTALL")
                    && root.join("node_modules/playwright/cli.js").exists()
                {
                    jobs.push(acropolis_npm::scripts::ScriptJob {
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
            let text = String::from_utf8(read_app_file(&ctx.opts.app_dir, "go.sum")?).context("go.sum is not UTF-8")?;
            let sum = acropolis_gomod::parse_go_sum(&text)?;
            let cache = acropolis_gomod::ModCache {
                root: ctx.cache_path("gomodcache"),
            };
            let proxy = ctx
                .opts
                .env
                .vars
                .get("GOPROXY")
                .map(|p| p.split([',', '|']).next().unwrap_or("").to_string())
                .filter(|p| p.starts_with("http"))
                .unwrap_or_else(|| "https://proxy.golang.org".to_string());
            if !ctx.opts.env.flag("ALLOW_PRIVATE_REGISTRY") {
                check_fetch_url("GOPROXY", &proxy)?;
            }
            let stats = acropolis_gomod::download_all(&ctx.fetcher, &sum, &cache, &proxy).await?;
            acropolis_events::log(
                &step.id,
                format!("{} modules, {} go.mod files", stats.modules, stats.gomods),
            );
            Ok(Out::None)
        }
        Action::BundleSpa { manager, lockfile, out } => {
            let opts = InstallOptions {
                include_dev: true,
                include_optional: true,
                platform: Default::default(),
            };
            let (m, app, lock) = (manager.clone(), ctx.opts.app_dir.clone(), lockfile.clone());
            let plan = tokio::task::spawn_blocking(move || install_plan_for(&m, &app, &lock, &opts)).await??;
            check_package_urls(&plan, &ctx.opts.env)?;
            let out = clean_relative(out)?.trim_end_matches('/').to_string();
            let root = ctx.work.join("bundle");
            let app = ctx.opts.app_dir.clone();
            let r2 = root.clone();
            tokio::task::spawn_blocking(move || {
                source::copy_tree(
                    &app,
                    &r2,
                    &Ignore::load(&app, &["**/node_modules".to_string(), "dist".to_string()]),
                )
            })
            .await??;
            let out_dir = root.join(&out);
            if matches!(out.as_str(), "" | ".") || symlink_on_the_way(&root, &out) {
                bail!("output directory {out:?} must stay inside the app directory");
            }
            match std::fs::symlink_metadata(&out_dir) {
                Ok(m) if m.is_dir() => std::fs::remove_dir_all(&out_dir)?,
                Ok(_) => std::fs::remove_file(&out_dir)?,
                Err(_) => {}
            }
            let env = ctx
                .opts
                .env
                .vars
                .iter()
                .filter(|(k, _)| !crate::detect::operator_key(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let res = acropolis_bundle::build_spa(acropolis_bundle::SpaInput {
                root: root.clone(),
                out_dir,
                plan: Arc::new(plan),
                fetcher: ctx.fetcher.clone(),
                base: "/".into(),
                env,
            })
            .await?;
            acropolis_events::log(
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
            let text = String::from_utf8(read_app_file(&ctx.opts.app_dir, "Cargo.lock")?)
                .context("Cargo.lock is not UTF-8")?;
            let lock = acropolis_cargo::parse_lock(&text)?;
            let vendor_dir = ctx.cache_path("cargo-vendor");
            let stats = acropolis_cargo::vendor(&ctx.fetcher, &lock, &vendor_dir).await?;
            let cargo_home = ctx.cache_path("cargo-home");
            std::fs::create_dir_all(&cargo_home)?;
            std::fs::write(
                cargo_home.join("config.toml"),
                acropolis_cargo::cargo_config(&vendor_dir),
            )?;
            acropolis_events::log(
                &step.id,
                format!("{} crates, {:.1} MB", stats.crates, stats.bytes as f64 / 1e6),
            );
            Ok(Out::None)
        }
        Action::Run {
            argv,
            env,
            network,
            cwd,
        } => {
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
            let root_bin = ctx.src.join("node_modules").join(".bin");
            if cwd_path != ctx.src && cwd_path.starts_with(&ctx.src) && root_bin.exists() {
                path.push(root_bin.to_string_lossy().into_owned());
            }
            for t in &tools {
                path.push(t.bin_dir.to_string_lossy().into_owned());
            }
            path.push("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            let home = ctx.cache_path("home");
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
                full_env.insert(
                    "GOMODCACHE".into(),
                    ctx.cache_path("gomodcache").to_string_lossy().into_owned(),
                );
                full_env.insert(
                    "GOCACHE".into(),
                    ctx.cache_path("gocache").to_string_lossy().into_owned(),
                );
                full_env.insert("GOPROXY".into(), "off".into());
                full_env.insert("GOSUMDB".into(), "off".into());
                full_env.insert("GOTOOLCHAIN".into(), "local".into());
                full_env.insert("GOFLAGS".into(), "-mod=readonly".into());
            }
            if tools.iter().any(|t| t.name == "rust") {
                let cargo_home = ctx.cache_path("cargo-home");
                std::fs::create_dir_all(&cargo_home)?;
                full_env.insert("CARGO_HOME".into(), cargo_home.to_string_lossy().into_owned());
                full_env.insert(
                    "CARGO_TARGET_DIR".into(),
                    ctx.cache_path("cargo-target").to_string_lossy().into_owned(),
                );
                full_env.insert("CARGO_TERM_COLOR".into(), "never".into());
                full_env.insert("CARGO_INCREMENTAL".into(), "0".into());
            }
            if let Some(node) = tools.iter().find(|t| t.name == "node") {
                let global = home.join(".npm-global");
                full_env.insert("npm_config_prefix".into(), global.to_string_lossy().into_owned());
                if let Some(p) = full_env.get_mut("PATH") {
                    *p = format!("{}:{p}", global.join("bin").display());
                }
                full_env.insert(
                    "npm_node_execpath".into(),
                    node.bin_dir.join("node").to_string_lossy().into_owned(),
                );
                full_env.insert("INIT_CWD".into(), cwd_path.to_string_lossy().into_owned());
            }
            for (k, v) in env {
                full_env.insert(k.clone(), ctx.subst(v));
            }
            if env.contains_key("ACROPOLIS_NFT_CACHE") {
                std::fs::write(ctx.work.join("acropolis-nft-cache.js"), NFT_CACHE_HOOK)?;
            }
            let argv: Vec<String> = argv.iter().map(|a| ctx.subst(a)).collect();
            let goflags_vendor = argv.iter().any(|a| a == "-mod=vendor");
            if goflags_vendor {
                full_env.remove("GOFLAGS");
            }
            let node_cache = ctx
                .cache
                .as_ref()
                .filter(|_| {
                    (cwd_path.starts_with(&ctx.src) || cwd_path.starts_with(&ctx.work))
                        && cwd_path.join("package.json").exists()
                })
                .map(|c| c.dir.clone());
            if let Some(c) = &node_cache {
                restore_node_caches(c, &cwd_path);
            }
            let res = ctx
                .exec
                .run(Cmd {
                    step: step.id.clone(),
                    argv,
                    cwd: cwd_path.clone(),
                    env: full_env,
                    network: *network,
                    writable: ctx.step_writable(),
                })
                .await;
            if let Some(c) = &node_cache {
                save_node_caches(c, &cwd_path);
            }
            res?;
            Ok(Out::None)
        }
        Action::ImageRun {
            image,
            commands,
            env,
            network,
            mount_app,
            after,
            tools,
            lowers,
        } => {
            let image = if image == "@base" {
                ctx.base()
                    .map(|b| b.reference.to_string())
                    .ok_or_else(|| anyhow!("no base image resolved"))?
            } else if let Some(variant) = image.strip_prefix("@base-variant:") {
                let (from, to) = variant
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid image alias {image}"))?;
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
            full_env
                .entry("PATH".into())
                .or_insert_with(|| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
            full_env.insert("HOME".into(), "/root".into());
            full_env.insert("SOURCE_DATE_EPOCH".into(), "0".into());
            for (k, v) in env {
                let v = ctx.subst(v);
                let placeholder = format!("${{{k}}}");
                let v = if v.contains(&placeholder) {
                    v.replace(&placeholder, full_env.get(k).map(|s| s.as_str()).unwrap_or(""))
                } else {
                    v
                };
                full_env.insert(k.clone(), v);
            }
            let mut binds = Vec::new();
            let installed = ctx.tools();
            let mut extra_path = Vec::new();
            for t in tools {
                let inst = installed
                    .iter()
                    .find(|i| &i.name == t)
                    .ok_or_else(|| anyhow!("toolchain {t} not installed"))?;
                let root = format!("/opt/acropolis/{}", t.replace(':', "-"));
                let rel = inst
                    .bin_dir
                    .strip_prefix(&inst.root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "bin".into());
                binds.push(acropolis_exec::rootfs::Bind {
                    host: inst.root.clone(),
                    guest: root.clone(),
                    readonly: true,
                });
                extra_path.push(format!("{root}/{rel}"));
            }
            if !extra_path.is_empty() {
                let p = full_env.get("PATH").cloned().unwrap_or_default();
                full_env.insert("PATH".into(), format!("{}:{p}", extra_path.join(":")));
            }
            if *mount_app {
                std::fs::create_dir_all(&ctx.src)?;
                binds.push(acropolis_exec::rootfs::Bind {
                    host: ctx.src.clone(),
                    guest: "/app".into(),
                    readonly: false,
                });
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
                        binds.push(acropolis_exec::rootfs::Bind {
                            host,
                            guest: guest.into(),
                            readonly: false,
                        });
                    }
                    if full_env.contains_key("UV_CACHE_DIR") {
                        full_env.insert("UV_CACHE_DIR".into(), "/root/.cache/uv".into());
                    }
                    full_env
                        .entry("COMPOSER_CACHE_DIR".into())
                        .or_insert_with(|| "/root/.cache/composer".into());
                    full_env
                        .entry("PIP_CACHE_DIR".into())
                        .or_insert_with(|| "/root/.cache/pip".into());
                }
            }
            let spec = acropolis_exec::rootfs::RootfsRun {
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
            acropolis_exec::rootfs::run(spec).await?;
            Ok(Out::Upper(upper))
        }
        Action::Layer { dest, from } => {
            let key = layer_cache_key(ctx, step, from);
            let cached = key.as_ref().and_then(|k| load_cached_layer(ctx, k));
            let reused = cached.is_some();
            let layer = match cached {
                Some(l) => l,
                None => build_layer(ctx, step, dest, from).await?,
            };
            if let Some(k) = &key
                && !reused
            {
                save_cached_layer(ctx, k, &layer);
            }
            if reused {
                acropolis_events::log(&step.id, "reused layer built from the same inputs");
            }
            acropolis_events::log(
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
            if let Some((k, _)) = spec.env.iter().find(|(_, v)| v.contains("{env:")) {
                bail!("internal error: image env {k} still holds an {{env:}} placeholder");
            }
            let mut labels = BTreeMap::new();
            labels.insert("dev.acropolis.plan".to_string(), ctx.plan.hash.clone());
            let patch = ConfigPatch {
                env: spec
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), ctx.subst_versions(v)))
                    .collect(),
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
                acropolis_events::log(&step.id, format!("wrote OCI layout {}", out.display()));
            }
            if let Some(target) = &ctx.opts.target {
                assemble::push_layers(&ctx.registry, target, &layers).await?;
                let digest = assemble::push_manifest(&ctx.registry, target, &assembled).await?;
                acropolis_events::emit(acropolis_events::Event::ImagePushed {
                    reference: target.to_string(),
                    digest: digest.clone(),
                });
                Ok(Out::Pushed(digest))
            } else {
                ctx.store.put_bytes("manifest", &assembled.manifest, None)?;
                ctx.store.put_bytes("config", &assembled.config, None)?;
                Ok(Out::Pushed(assembled.manifest_digest))
            }
        }
    }
}

fn contained(base: &Path, path: &Path) -> Result<()> {
    let real = std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
    let root = std::fs::canonicalize(base).with_context(|| format!("resolving {}", base.display()))?;
    if !real.starts_with(&root) {
        bail!(
            "{} resolves to {}, outside of {}",
            path.display(),
            real.display(),
            root.display()
        );
    }
    Ok(())
}

fn read_app_file(app_dir: &Path, rel: &str) -> Result<Vec<u8>> {
    let path = app_dir.join(rel);
    contained(app_dir, &path)?;
    std::fs::read(&path).with_context(|| format!("reading {rel}"))
}

fn toolchain_dir(home: &Path, name: &str) -> Result<PathBuf> {
    if name.contains(['/', '\0']) || name.starts_with('.') {
        bail!("invalid toolchain version in {name:?}");
    }
    Ok(home.join("toolchains").join(name))
}

fn symlink_on_the_way(root: &Path, rel: &str) -> bool {
    let mut cur = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        cur.push(part);
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

fn clean_relative(p: &str) -> Result<&str> {
    let t = p.trim_start_matches("./");
    if Path::new(t).is_absolute()
        || Path::new(t)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        bail!("path {p:?} must stay inside the app directory");
    }
    Ok(t)
}

use acropolis_fetch::private_host;

fn check_fetch_url(what: &str, url: &str) -> Result<()> {
    let Ok(u) = url::Url::parse(url) else { return Ok(()) };
    if u.scheme() == "http" {
        bail!("{what} is fetched over plain http ({url}); use https or set ACROPOLIS_ALLOW_PRIVATE_REGISTRY=1");
    }
    if u.host_str().is_some_and(private_host) {
        bail!(
            "{what} points at a private or link-local address ({url}); set ACROPOLIS_ALLOW_PRIVATE_REGISTRY=1 if this registry is intended"
        );
    }
    Ok(())
}

fn check_package_urls(plan: &InstallPlan, env: &crate::Env) -> Result<()> {
    if env.flag("ALLOW_PRIVATE_REGISTRY") {
        return Ok(());
    }
    for p in &plan.packages {
        if let acropolis_npm::Source::Registry { url, .. } | acropolis_npm::Source::Git { url } = &p.source {
            check_fetch_url(&p.path, url)?;
        }
    }
    Ok(())
}

pub fn patched_dependencies(app_dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(pj) = crate::detect::read_package_json(app_dir) {
        for v in [
            pj.get("patchedDependencies"),
            pj.get("pnpm").and_then(|p| p.get("patchedDependencies")),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(m) = v.as_object() {
                out.extend(
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|f| (k.clone(), f.to_string()))),
                );
            }
        }
    }
    if let Some(text) = read_app_file(app_dir, "pnpm-workspace.yaml")
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
    {
        let mut inside = false;
        for line in text.lines() {
            if !line.starts_with(' ') && !line.starts_with('\t') {
                inside = line.trim_end() == "patchedDependencies:";
                continue;
            }
            if inside && let Some((k, v)) = line.trim().split_once(": ") {
                out.push((
                    k.trim_matches(['"', '\'']).to_string(),
                    v.trim().trim_matches(['"', '\'']).to_string(),
                ));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn apply_patched_dependencies(app_dir: &Path, plan: &InstallPlan, root: &Path) -> Result<Vec<String>> {
    let mut log = Vec::new();
    for (spec, file) in patched_dependencies(app_dir) {
        let rel = clean_relative(&file)?;
        let diff = read_app_file(app_dir, rel)
            .and_then(|b| Ok(String::from_utf8(b)?))
            .with_context(|| format!("reading patch {file} for {spec}"))?;
        let (name, version) = acropolis_npm::patch::targets(&spec);
        let mut applied = 0;
        for p in plan
            .packages
            .iter()
            .filter(|p| p.name == name && version.as_ref().is_none_or(|v| *v == p.version))
        {
            let dir = root.join(&p.path);
            if dir.is_dir() {
                contained(root, &dir)?;
                acropolis_npm::patch::apply(&diff, &dir).with_context(|| format!("applying {file} to {}", p.path))?;
                applied += 1;
            }
        }
        if applied == 0 {
            log.push(format!("warning: patch {file} matches no installed {spec}"));
        } else {
            log.push(format!("patched {spec} ({applied} copies) with {file}"));
        }
    }
    Ok(log)
}

fn keep_packages(mut plan: InstallPlan, full: InstallPlan, keep: &[String]) -> InstallPlan {
    let have: std::collections::BTreeSet<String> = plan.packages.iter().map(|p| p.path.clone()).collect();
    let roots: Vec<String> = keep.iter().map(|k| format!("node_modules/{k}")).collect();
    for p in full.packages {
        if keep.contains(&p.name) && !have.contains(&p.path) {
            plan.packages.push(p);
        }
    }
    for l in full.links {
        if roots.contains(&l.path) && !plan.links.iter().any(|x| x.path == l.path) {
            plan.links.push(l);
        }
    }
    plan
}

pub fn install_plan_for(manager: &str, app_dir: &Path, lockfile: &str, opts: &InstallOptions) -> Result<InstallPlan> {
    Lock::read(manager, app_dir, lockfile)?.plan(opts, &[])
}

/// A lockfile parsed once, so several install plans (prod, full) can be derived from it.
enum Lock {
    Npm(PackageLock),
    Pnpm(acropolis_npm::pnpm::PnpmLock),
    Yarn(
        acropolis_npm::yarn::YarnLock,
        serde_json::Value,
        Vec<(String, serde_json::Value)>,
    ),
    Bun(acropolis_npm::bun::BunLock),
}

impl Lock {
    fn read(manager: &str, app_dir: &Path, lockfile: &str) -> Result<Self> {
        let bytes = read_app_file(app_dir, lockfile)?;
        Ok(match manager {
            "npm" => Lock::Npm(PackageLock::parse(&bytes)?),
            "pnpm" => {
                let text = String::from_utf8(bytes).context("pnpm-lock.yaml is not UTF-8")?;
                Lock::Pnpm(acropolis_npm::pnpm::PnpmLock::parse(&text)?)
            }
            "yarn" => {
                let text = String::from_utf8(bytes).context("yarn.lock is not UTF-8")?;
                let pj: serde_json::Value = serde_json::from_slice(&read_app_file(app_dir, "package.json")?)?;
                let ws = acropolis_npm::yarn::expand_workspaces(app_dir, &pj);
                Lock::Yarn(acropolis_npm::yarn::YarnLock::parse(&text)?, pj, ws)
            }
            "bun" => {
                let text = String::from_utf8(bytes).context("bun.lock is not UTF-8")?;
                Lock::Bun(acropolis_npm::bun::BunLock::parse(&text)?)
            }
            other => bail!("unsupported package manager {other}"),
        })
    }

    fn plan(&self, opts: &InstallOptions, workspaces: &[String]) -> Result<InstallPlan> {
        match self {
            Lock::Npm(lock) => InstallPlan::from_lock(lock, opts),
            Lock::Pnpm(lock) => lock.install_plan(opts, &[]),
            Lock::Yarn(lock, pj, ws) => lock.install_plan(pj, ws, opts),
            Lock::Bun(lock) => lock.install_plan_scoped(opts, workspaces),
        }
    }
}

/// Install plan from the lockfile (blocking: reads and parses it once). `None` when an npm
/// lockfile is out of sync with package.json and the registry must resolve instead.
fn locked_plan(
    manager: &str,
    app_dir: &Path,
    lockfile: &str,
    opts: &InstallOptions,
    workspaces: &[String],
    keep: &[String],
) -> Result<Option<InstallPlan>> {
    let lock = match Lock::read(manager, app_dir, lockfile) {
        Ok(lock) => lock,
        // Valid JSON that is not lockfile-shaped is still ignored when package.json moved on.
        Err(e) => {
            let raw = (manager == "npm")
                .then(|| read_app_file(app_dir, lockfile).ok())
                .flatten()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
            let root = raw.as_ref().and_then(|v| v.get("packages")).and_then(|p| p.get(""));
            return if raw.is_some() && npm_lock_out_of_sync(app_dir, root) {
                Ok(None)
            } else {
                Err(e)
            };
        }
    };
    if let Lock::Npm(l) = &lock {
        let root = l.packages.get("").map(|r| {
            serde_json::json!({
                "dependencies": r.dependencies,
                "devDependencies": r.dev_dependencies,
                "optionalDependencies": r.optional_dependencies,
            })
        });
        if npm_lock_out_of_sync(app_dir, root.as_ref()) {
            return Ok(None);
        }
    }
    let plan = lock.plan(opts, workspaces)?;
    if keep.is_empty() || opts.include_dev {
        return Ok(Some(plan));
    }
    let full = InstallOptions {
        include_dev: true,
        ..opts.clone()
    };
    Ok(Some(keep_packages(plan, lock.plan(&full, workspaces)?, keep)))
}

async fn run_lifecycle(
    ctx: &Arc<Ctx>,
    step: &Step,
    root: &Path,
    jobs: &[acropolis_npm::scripts::ScriptJob],
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let tools = ctx.tools();
    let node = tools
        .iter()
        .find(|t| t.name == "node")
        .ok_or_else(|| anyhow!("install scripts need the node toolchain"))?;
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
                npm_dir
                    .join("node_modules/@npmcli/run-script/lib/node-gyp-bin")
                    .to_string_lossy()
                    .into_owned(),
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
            env.insert(
                "npm_config_nodedir".to_string(),
                node.root.to_string_lossy().into_owned(),
            );
            env.insert(
                "npm_config_node_gyp".to_string(),
                npm_dir
                    .join("node_modules/node-gyp/bin/node-gyp.js")
                    .to_string_lossy()
                    .into_owned(),
            );
            env.insert(
                "npm_config_user_agent".to_string(),
                format!("npm/10 node/v{} linux x64", node.version),
            );
            env.insert(
                "npm_node_execpath".to_string(),
                node.bin_dir.join("node").to_string_lossy().into_owned(),
            );
            env.insert("INIT_CWD".to_string(), root.to_string_lossy().into_owned());
            env.insert(
                "PUPPETEER_CACHE_DIR".to_string(),
                root.join("node_modules/.cache/puppeteer")
                    .to_string_lossy()
                    .into_owned(),
            );
            env.insert(
                "PLAYWRIGHT_BROWSERS_PATH".to_string(),
                root.join("node_modules/.cache/ms-playwright")
                    .to_string_lossy()
                    .into_owned(),
            );
            env.insert("NODE_ENV".to_string(), "production".to_string());
            acropolis_events::log(&step.id, format!("{} {stage}: {command}", job.name));
            ctx.exec
                .run(Cmd {
                    step: step.id.clone(),
                    argv: vec!["/bin/sh".into(), "-c".into(), command.clone()],
                    cwd: pkg_dir.clone(),
                    env,
                    network: true,
                    writable: ctx.step_writable(),
                })
                .await
                .with_context(|| format!("{stage} script of {}", job.name))?;
        }
    }
    Ok(())
}

async fn resolve_alias(ctx: &Arc<Ctx>, image: &str) -> Result<String> {
    let Some(build_image) = image.strip_prefix("@slim-of:") else {
        return Ok(image.to_string());
    };
    let (layers, _) = image_rootfs(ctx, build_image).await?;
    for dir in layers.iter().rev() {
        for rel in ["etc/os-release", "usr/lib/os-release"] {
            let Ok(text) = std::fs::read_to_string(dir.join(rel)) else {
                continue;
            };
            let field = |k: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(k))
                    .map(|v| v.trim_matches('"').to_string())
            };
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
    if let Some(list) = resolved
        .config
        .get("config")
        .and_then(|c| c.get("Env"))
        .and_then(|e| e.as_array())
    {
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
            if !acropolis_oci::reference::valid_digest(&d.digest) {
                bail!("invalid layer digest {:?} in {}", d.digest, reference);
            }
            let hex = d.digest.trim_start_matches("sha256:").to_string();
            let rootfs_root = std::env::var("ACROPOLIS_ROOTFS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| ctx.opts.home.join("rootfs"));
            std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
                .create(&rootfs_root)?;
            let dir = rootfs_root.join(&hex);
            let marker = rootfs_root.join(format!("{hex}.complete"));
            if marker.exists() {
                crate::gc::touch(&marker);
                return Ok::<PathBuf, anyhow::Error>(dir);
            }
            let staged = staging(&dir);
            let (url, headers) = ctx.registry.blob_location(&reference, &d.digest).await?;
            let expected = acropolis_store::Integrity::parse_oci(&d.digest)?;
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
                    acropolis_oci::unpack::unpack_for_overlay(reader, &dir2)
                })
                .await?;
            if std::fs::rename(&staged, &dir).is_err() {
                let _ = std::fs::remove_dir_all(&staged);
            }
            if !dir.is_dir() {
                bail!("could not place image layer {} at {}", d.digest, dir.display());
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

/// `root` is the lockfile's `packages[""]` entry.
fn npm_lock_out_of_sync(app_dir: &Path, root: Option<&serde_json::Value>) -> bool {
    let Ok(pj) = crate::detect::read_package_json(app_dir) else {
        return false;
    };
    ["dependencies", "devDependencies", "optionalDependencies"]
        .into_iter()
        .any(|k| {
            let have = root.and_then(|r| r.get(k)).and_then(|d| d.as_object());
            pj.get(k).and_then(|d| d.as_object()).is_some_and(|want| {
                want.iter()
                    .any(|(name, range)| have.and_then(|h| h.get(name)) != Some(range))
            })
        })
}

fn with_config_excludes(ctx: &Ctx, base: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    if let Some(extra) = ctx.opts.env.vars.get("ACROPOLIS_EXCLUDE") {
        out.extend(extra.lines().map(|l| l.to_string()));
    }
    out
}

fn staging(dest: &Path) -> PathBuf {
    use std::hash::{BuildHasher, Hasher};
    let nonce = std::collections::hash_map::RandomState::new().build_hasher().finish();
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!(".{name}.staging-{}-{nonce:016x}", std::process::id()))
}

const GO_STD_CACHE: &str = ".acropolis-std-cgo0";

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
        bail!(
            "precompiling the Go standard library failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    std::fs::write(staged.join(".complete"), "")?;
    if std::fs::rename(&staged, root.join(GO_STD_CACHE)).is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    acropolis_events::log(
        &step.id,
        format!(
            "precompiled the Go standard library in {:.1}s",
            started.elapsed().as_secs_f64()
        ),
    );
    Ok(())
}

fn seed_go_cache(go_root: &Path, gocache: &Path) {
    let std_cache = go_root.join(GO_STD_CACHE);
    if !std_cache.join(".complete").exists() {
        return;
    }
    let empty = std::fs::read_dir(gocache)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(true);
    if !empty {
        return;
    }
    let ig = crate::ignore::Ignore::new(&[".complete".to_string()]);
    let _ = source::copy_tree(&std_cache, gocache, &ig);
}

fn publish(staged: &Path, dest: &Path) -> Result<()> {
    std::fs::write(staged.join(".acropolis-complete"), "")?;
    for _ in 0..3 {
        if dest.join(".acropolis-complete").exists() {
            let _ = std::fs::remove_dir_all(staged);
            return Ok(());
        }
        if dest.exists() {
            let stale = PathBuf::from(format!("{}.stale", staging(dest).display()));
            if std::fs::rename(dest, &stale).is_ok() {
                std::thread::spawn(move || {
                    let _ = std::fs::remove_dir_all(stale);
                });
            }
        }
        match std::fs::rename(staged, dest) {
            Ok(()) => return Ok(()),
            Err(_) if dest.join(".acropolis-complete").exists() => {
                let _ = std::fs::remove_dir_all(staged);
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    bail!("could not publish {}", dest.display())
}

fn image_cache_path(ctx: &Ctx, image: &str) -> PathBuf {
    let key = acropolis_store::sha256_bytes(format!("{image}|{:?}", ctx.opts.platform).as_bytes()).hex();
    ctx.opts.home.join("cache").join("images").join(&key[..24])
}

fn load_cached_image(ctx: &Ctx, image: &str) -> Option<ResolvedImage> {
    let base = image_cache_path(ctx, image);
    let meta = std::fs::metadata(base.with_extension("json")).ok()?;
    let ttl = ctx
        .opts
        .env
        .config("TAG_TTL")
        .and_then(|(v, _)| v.parse::<u64>().ok())
        .unwrap_or(900);
    let pinned = image.contains("@sha256:");
    let age = meta.modified().ok()?.elapsed().ok()?;
    if !pinned && (ttl == 0 || age.as_secs() > ttl) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(base.with_extension("json")).ok()?).ok()?;
    let config_raw = bytes::Bytes::from(std::fs::read(base.with_extension("config")).ok()?);
    let config_digest = acropolis_store::sha256_bytes(&config_raw).to_oci();
    let manifest: acropolis_oci::image::Manifest = serde_json::from_value(v.get("manifest")?.clone()).ok()?;
    if manifest.config.digest != config_digest
        || !manifest
            .layers
            .iter()
            .all(|l| acropolis_oci::reference::valid_digest(&l.digest))
    {
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

fn missing_manifest(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("MANIFEST_UNKNOWN") || text.contains("NAME_UNKNOWN") || text.contains("404")
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
        acropolis_events::log(&step.id, format!("{} -> {} (cached)", image, resolved.manifest_digest));
        return Ok(Out::Base(Arc::new(resolved)));
    }
    let r = Reference::parse(image)?;
    let (reference, manifest, digest) = match ctx.registry.resolve_manifest(&r, &ctx.opts.platform).await {
        Err(e) if image.contains("-trixie-slim") && format!("{e:#}").contains("MANIFEST_UNKNOWN") => {
            let fallback = image.replace("-trixie-slim", "-bookworm-slim");
            acropolis_events::log(&step.id, format!("warning: {image} does not exist, using {fallback}"));
            ctx.registry
                .resolve_manifest(&Reference::parse(&fallback)?, &ctx.opts.platform)
                .await?
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
    acropolis_events::log(&step.id, format!("{} -> {}", image, resolved.manifest_digest));
    Ok(Out::Base(Arc::new(resolved)))
}

async fn write_oci_layout(
    ctx: &Arc<Ctx>,
    base: Option<&ResolvedImage>,
    layers: &[Layer],
    a: &assemble::Assembled,
    out: &Path,
) -> Result<()> {
    let mut blobs: Vec<(String, PathBuf)> = Vec::new();
    if let Some(b) = base {
        let futs = b.manifest.layers.iter().map(|d| async move {
            let (url, headers) = ctx.registry.blob_location(&b.reference, &d.digest).await?;
            let expected = acropolis_store::Integrity::parse_oci(&d.digest)?;
            let blob = ctx.fetcher.blob_with(&d.digest, &url, &headers, Some(expected)).await?;
            Ok::<_, anyhow::Error>((d.digest.trim_start_matches("sha256:").to_string(), blob.path))
        });
        blobs.extend(futures::future::try_join_all(futs).await?);
    }
    for l in layers {
        blobs.push((l.digest.hex(), l.path.clone()));
    }
    let mut desc = serde_json::json!({
        "mediaType": acropolis_oci::image::MT_OCI_MANIFEST,
        "digest": a.manifest_digest,
        "size": a.manifest.len(),
    });
    if let Some(t) = &ctx.opts.target {
        desc["annotations"] = serde_json::json!({ "org.opencontainers.image.ref.name": t.tag.clone().unwrap_or_else(|| "latest".into()), "io.containerd.image.name": t.to_string() });
    }
    let index =
        serde_json::json!({ "schemaVersion": 2, "mediaType": acropolis_oci::image::MT_OCI_INDEX, "manifests": [desc] });
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

fn layer_cache_key(ctx: &Arc<Ctx>, step: &Step, from: &LayerFrom) -> Option<String> {
    if !matches!(
        from,
        LayerFrom::Tool { .. } | LayerFrom::ToolTree { .. } | LayerFrom::NodeModules { .. } | LayerFrom::Inline { .. }
    ) {
        return None;
    }
    let mut versions: Vec<String> = ctx
        .dep_outputs(step)
        .into_iter()
        .filter_map(|o| {
            if let Out::Tool(t) = o {
                Some(format!("{}={}", t.name, t.version))
            } else {
                None
            }
        })
        .collect();
    versions.sort();
    let o = ctx.opts.layer;
    let text = format!(
        "v1 {} {:?} {} {} {}",
        step.hash,
        o.compression,
        o.level,
        versions.join(","),
        step.id
    );
    Some(acropolis_store::sha256_bytes(text.as_bytes()).hex())
}

fn load_cached_layer(ctx: &Arc<Ctx>, key: &str) -> Option<Layer> {
    let text = std::fs::read_to_string(ctx.opts.home.join("layer-cache").join(format!("{key}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let digest = acropolis_store::Integrity::parse_oci(v["digest"].as_str()?).ok()?;
    let diff_id = acropolis_store::Integrity::parse_oci(v["diff_id"].as_str()?).ok()?;
    let blob = ctx.store.get(&digest)?;
    crate::gc::touch(&blob.path);
    Some(Layer {
        digest,
        diff_id,
        size: v["size"].as_u64()?,
        uncompressed_size: v["uncompressed_size"].as_u64()?,
        media_type: v["media_type"].as_str()?.to_string(),
        path: blob.path,
        comment: v["comment"].as_str().unwrap_or("").to_string(),
    })
}

fn save_cached_layer(ctx: &Arc<Ctx>, key: &str, l: &Layer) {
    let dir = ctx.opts.home.join("layer-cache");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let v = serde_json::json!({
        "digest": l.digest.to_oci(),
        "diff_id": l.diff_id.to_oci(),
        "size": l.size,
        "uncompressed_size": l.uncompressed_size,
        "media_type": l.media_type,
        "comment": l.comment,
    });
    let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(format!("{key}.json")));
    }
}

async fn github_latest_tag(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    acropolis_fetch::ensure_tls();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    acropolis_events::add_request();
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
            let root = ctx.opts.app_dir.join(clean_relative(path)?);
            if !root.exists() {
                bail!("{} does not exist", root.display());
            }
            contained(&ctx.opts.app_dir, &root)?;
            let ex = exclude.clone();
            tokio::task::spawn_blocking(move || {
                let ig = Ignore::new(&ex);
                source::dir_layer(&store, &root, &ig, &dest, &comment, opts)
            })
            .await?
        }
        LayerFrom::WorkDir { path, exclude } => {
            let (base, root) = if path == "." {
                (ctx.src.clone(), ctx.src.clone())
            } else if let Some(rest) = path.strip_prefix("@work/") {
                (ctx.work.clone(), ctx.work.join(clean_relative(rest)?))
            } else {
                (ctx.src.clone(), ctx.src.join(clean_relative(path)?))
            };
            if !root.exists() {
                bail!("build output {} does not exist", root.display());
            }
            contained(&base, &root)?;
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
                    let root = src.join(clean_relative(from)?);
                    if !root.exists() {
                        continue;
                    }
                    contained(&src, &root)?;
                    if root.is_file() {
                        let prefix = if dest.is_empty() {
                            to.clone()
                        } else {
                            format!("{dest}/{to}")
                        };
                        let mut tw = TarWriter::new(Vec::new());
                        let parent = Path::new(&prefix)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        for d in source::ancestors(&parent)
                            .into_iter()
                            .skip(source::ancestors(&dest).len())
                        {
                            tw.dir(&d, 0o755)?;
                        }
                        let mut f = std::fs::File::open(&root)?;
                        let meta = f.metadata()?;
                        use std::os::unix::fs::PermissionsExt;
                        tw.file_reader(
                            &prefix,
                            if meta.permissions().mode() & 0o111 != 0 {
                                0o755
                            } else {
                                0o644
                            },
                            meta.len(),
                            &mut f,
                        )?;
                        std::io::Write::write_all(&mut b, tw.get_mut())?;
                        continue;
                    }
                    let prefix = if dest.is_empty() {
                        to.clone()
                    } else {
                        format!("{dest}/{to}")
                    };
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
            if std::fs::symlink_metadata(&file).is_ok_and(|m| !m.is_file()) {
                bail!("{} is not a regular file", file.display());
            }
            let mut bases = vec![ctx.work.clone(), ctx.src.clone()];
            if let Some(c) = &ctx.cache {
                bases.push(c.dir.clone());
            }
            if !bases.iter().any(|b| contained(b, &file).is_ok()) {
                bail!("{} is outside the build directories", file.display());
            }
            let mode = *mode;
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(layer::LayerBuilder::new(&store, &comment, opts)?);
                let parent = Path::new(&dest)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                for d in source::ancestors(&parent) {
                    tw.dir(&d, 0o755)?;
                }
                let mut f = std::fs::File::open(&file).with_context(|| format!("opening {}", file.display()))?;
                let size = f.metadata()?.len();
                tw.file_reader(&dest, mode, size, &mut f)?;
                tw.into_inner().finish()
            })
            .await?
        }
        LayerFrom::Tool { tool, files } => {
            let t = ctx
                .tools()
                .into_iter()
                .find(|t| &t.name == tool)
                .ok_or_else(|| anyhow!("toolchain {tool} not installed"))?;
            let files = files.clone();
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(layer::LayerBuilder::new(&store, &comment, opts)?);
                let mut dirs = std::collections::BTreeSet::new();
                for (_, to) in &files {
                    let full = if dest.is_empty() {
                        to.clone()
                    } else {
                        format!("{dest}/{to}")
                    };
                    let parent = Path::new(&full)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    for d in source::ancestors(&parent) {
                        dirs.insert(d);
                    }
                }
                for d in &dirs {
                    tw.dir(d, 0o755)?;
                }
                for (from, to) in &files {
                    let full = if dest.is_empty() {
                        to.clone()
                    } else {
                        format!("{dest}/{to}")
                    };
                    let src = t.root.join(from);
                    let mut f = std::fs::File::open(&src).with_context(|| format!("opening {}", src.display()))?;
                    let size = f.metadata()?.len();
                    tw.file_reader(&full, 0o755, size, &mut f)?;
                }
                tw.into_inner().finish()
            })
            .await?
        }
        LayerFrom::Upper {
            step: from_step,
            include,
            exclude,
        } => {
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
            let t = ctx
                .tools()
                .into_iter()
                .find(|t| &t.name == tool)
                .ok_or_else(|| anyhow!("toolchain {tool} not installed"))?;
            tokio::task::spawn_blocking(move || {
                let entries = source::walk(
                    &t.root,
                    &Ignore::new(&[".acropolis-complete".into(), ".acropolis-std-*".into()]),
                )?;
                let mut b = layer::LayerBuilder::new(&store, &comment, opts)?;
                source::stream_tree_into(&t.root, &entries, &dest, true, &mut b)?;
                b.finish()
            })
            .await?
        }
        LayerFrom::NodeShim => {
            let base = ctx.base().map(|b| b.reference.to_string()).unwrap_or_default();
            let shell = base.contains("/distroless/");
            let node = base.contains("/distroless/nodejs");
            tokio::task::spawn_blocking(move || {
                let mut tw = TarWriter::new(Vec::new());
                if shell {
                    tw.symlink("bin/sh", "/busybox/sh")?;
                    tw.symlink("usr/bin/env", "/busybox/env")?;
                }
                if node {
                    tw.dir("usr/local", 0o755)?;
                    tw.dir("usr/local/bin", 0o755)?;
                    tw.symlink("usr/local/bin/node", "/nodejs/bin/node")?;
                }
                layer::from_fragments(&store, &comment, vec![std::mem::take(tw.get_mut())], opts)
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
                    let p = if dest.is_empty() {
                        name.clone()
                    } else {
                        format!("{dest}/{name}")
                    };
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
                    let rel = clean_relative(path.trim_matches('/'))?;
                    let target = if dest.is_empty() {
                        rel.to_string()
                    } else {
                        format!("{dest}/{rel}")
                    };
                    let found: Vec<PathBuf> = layers
                        .iter()
                        .filter(|l| !symlink_on_the_way(l, rel))
                        .map(|l| l.join(rel))
                        .filter(|p| std::fs::symlink_metadata(p).is_ok())
                        .collect();
                    if found.is_empty() {
                        bail!("{rel} not found in {image}");
                    }
                    let mut head = TarWriter::new(Vec::new());
                    let parent = Path::new(&target)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
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
                acropolis_npm::install::stream_node_modules(
                    &state.plan,
                    &state.tarballs,
                    &dest,
                    |_| true,
                    &mut |frag| {
                        std::io::Write::write_all(&mut b, &frag)?;
                        Ok(())
                    },
                )?;
                b.finish()
            })
            .await?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_versions_cannot_leave_the_toolchains_dir() {
        let home = Path::new("/h");
        assert_eq!(
            toolchain_dir(home, "yarn-berry-4.1.0").unwrap(),
            PathBuf::from("/h/toolchains/yarn-berry-4.1.0")
        );
        assert!(toolchain_dir(home, "yarn-berry-../../../../4.1.0").is_err());
        assert!(toolchain_dir(home, "mise-v1/../../x").is_err());
        let dest = home.join("toolchains/node-22.1.0");
        assert_ne!(staging(&dest), staging(&dest));
    }

    #[test]
    fn private_hosts_include_ipv4_embedded_in_ipv6() {
        for h in [
            "169.254.169.254",
            "[::ffff:169.254.169.254]",
            "[::ffff:a9fe:a9fe]",
            "[64:ff9b::a9fe:a9fe]",
            "[::ffff:10.0.0.5]",
            "localhost",
            "[fd00::1]",
        ] {
            assert!(private_host(h), "{h}");
        }
        for h in [
            "registry.npmjs.org",
            "8.8.8.8",
            "[64:ff9b::808:808]",
            "[2606:4700::1111]",
        ] {
            assert!(!private_host(h), "{h}");
        }
        assert!(check_fetch_url("GOPROXY", "https://[::ffff:169.254.169.254]/").is_err());
        assert!(check_fetch_url("GOPROXY", "http://proxy.example.com").is_err());
        assert!(check_fetch_url("GOPROXY", "https://proxy.golang.org").is_ok());
    }

    #[test]
    fn shared_step_errors_keep_the_command_class() {
        use std::os::unix::process::ExitStatusExt;
        let failed = acropolis_exec::CommandFailed::new(
            &["/bin/sh".into(), "-c".into(), "vite build".into()],
            std::process::ExitStatus::from_raw(1 << 8),
            vec!["FATAL ERROR: JavaScript heap out of memory".into()],
        );
        let original = anyhow::Error::new(failed).context("step build (run vite build)");
        let text = format!("{original:#}");
        let shared = anyhow::Error::new(StepError(Arc::new(original)));
        assert_eq!(format!("{shared:#}"), text);
        assert_eq!(crate::errors::classify(&shared), crate::errors::ErrorClass::User);
    }

    #[test]
    fn app_files_behind_symlinks_out_of_the_app_are_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(tmp.path().join("secret.json"), br#"{"auths":{}}"#).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret.json"), app.join("package-lock.json")).unwrap();
        let opts = InstallOptions {
            include_dev: true,
            include_optional: true,
            platform: Default::default(),
        };
        let err = install_plan_for("npm", &app, "package-lock.json", &opts).unwrap_err();
        assert_eq!(
            crate::errors::classify(&err),
            crate::errors::ErrorClass::Config,
            "{err:#}"
        );
        std::fs::write(app.join("ok.lock"), b"x").unwrap();
        assert_eq!(read_app_file(&app, "ok.lock").unwrap(), b"x");
    }

    #[test]
    fn yarn2_pins_cover_the_default_and_parse() {
        assert!(YARN2_SHA256.iter().any(|(v, _)| *v == "2.4.3"));
        for (_, sha) in YARN2_SHA256 {
            acropolis_store::Integrity::parse_oci(&format!("sha256:{sha}")).unwrap();
        }
    }

    #[test]
    fn npm_lock_sync_is_judged_from_the_single_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path();
        let opts = InstallOptions {
            include_dev: false,
            include_optional: true,
            platform: Default::default(),
        };
        let plan = |lock: &str| {
            std::fs::write(app.join("package-lock.json"), lock).unwrap();
            locked_plan("npm", app, "package-lock.json", &opts, &[], &[])
        };
        std::fs::write(app.join("package.json"), r#"{"dependencies":{"a":"^1.0.0"}}"#).unwrap();
        let a = r#""node_modules/a":{"version":"1.0.0","resolved":"https://r/a.tgz","integrity":"sha512-M82xg3uZnE836u68o/DpLtSQjxHUoYkuK82v4psznrFjZO3msYD6qb/wAhGD9KdkO/puwfnDhOc9imA3NDrp9g=="}"#;
        let synced = plan(&format!(
            r#"{{"lockfileVersion":3,"packages":{{"":{{"dependencies":{{"a":"^1.0.0"}}}},{a}}}}}"#
        ));
        assert_eq!(synced.unwrap().unwrap().packages.len(), 1);
        let stale = plan(&format!(
            r#"{{"lockfileVersion":3,"packages":{{"":{{"dependencies":{{"a":"^0.9.0"}}}},{a}}}}}"#
        ));
        assert!(stale.unwrap().is_none());
        // Not lockfile-shaped: ignored when out of sync, an error otherwise.
        assert!(plan(r#"{"packages":{"":{"dependencies":{"a":1}}}}"#).unwrap().is_none());
        assert!(plan(r#"{"packages":{"":{"dependencies":{"a":"^1.0.0"}},"x":{"dependencies":{"b":1}}}}"#).is_err());
        assert!(plan("not json").is_err());
    }

    #[test]
    fn node_modules_restore_stays_inside_src() {
        let tmp = tempfile::tempdir().unwrap();
        let (cache, src) = (tmp.path().join("cache"), tmp.path().join("src"));
        std::fs::create_dir_all(cache.join("nm-prev/0")).unwrap();
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(cache.join("nm-prev/.key"), "k").unwrap();
        std::fs::write(cache.join("nm-prev/.list"), "0\t../escape/node_modules").unwrap();
        assert!(!restore_node_modules(&cache, &src, "k"));
        assert!(!tmp.path().join("escape").exists());
        std::os::unix::fs::symlink(tmp.path(), src.join("pkg")).unwrap();
        std::fs::write(cache.join("nm-prev/.list"), "0\tpkg/node_modules").unwrap();
        assert!(!restore_node_modules(&cache, &src, "k"));
        assert!(!tmp.path().join("node_modules").exists());
    }

    struct Sleeper;

    impl Executor for Sleeper {
        fn name(&self) -> &'static str {
            "sleeper"
        }
        fn hermetic(&self) -> bool {
            false
        }
        fn run(&self, _cmd: Cmd) -> BoxFuture<'_, Result<acropolis_exec::Output>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(acropolis_exec::Output { tail: vec![] })
            })
        }
    }

    #[tokio::test]
    async fn build_timeout_fails_the_build_instead_of_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        let mut b = crate::plan::PlanBuilder::new("t", "test");
        for i in 0..8 {
            b.step(
                &format!("s{i}"),
                "sleep",
                Action::Run {
                    argv: vec!["true".into()],
                    env: BTreeMap::new(),
                    network: false,
                    cwd: "@app".into(),
                },
                &[],
            );
        }
        let mut env = Env::default();
        env.vars.insert("ACROPOLIS_BUILD_TIMEOUT".into(), "1".into());
        env.vars.insert("ACROPOLIS_NO_CACHE".into(), "1".into());
        let opts = BuildOptions {
            app_dir: app,
            home: tmp.path().join("home"),
            target: None,
            env,
            layer: LayerOptions::default(),
            mirrors: vec![],
            concurrency: 2,
            keep_work: false,
            platform: acropolis_oci::image::host_platform(),
            oci_out: None,
        };
        let err = execute(b.finish(), opts, Arc::new(Sleeper)).await.unwrap_err();
        assert!(format!("{err:#}").contains("ACROPOLIS_BUILD_TIMEOUT"), "{err:#}");
    }
}
