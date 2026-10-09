use acropolis_fetch::Fetcher;
use acropolis_npm::install::Inside;
use acropolis_npm::{InstallPackage, InstallPlan};
use anyhow::{Context, Result, anyhow, bail};
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
};
use rolldown::{Bundler, BundlerOptions, InputItem, ModuleType, OutputFormat, Platform, RawMinifyOptions};
use rolldown_common::side_effects::HookSideEffects;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::OnceCell;

pub mod pkgindex;

use pkgindex::PkgIndex;

struct LazyPackages {
    started: Instant,
    root: PathBuf,
    inside: Inside,
    by_path: HashMap<String, InstallPackage>,
    fetcher: Fetcher,
    cells: Mutex<HashMap<String, Arc<OnceCell<Arc<PkgIndex>>>>>,
    full: Mutex<std::collections::HashSet<String>>,
    stats: Mutex<Stats>,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub last_fetch_ms: u64,
    pub extract_ms: u64,
    pub packages_fetched: usize,
    pub bytes_fetched: u64,
    pub packages_in_lockfile: usize,
    pub files_written: usize,
    pub full_extractions: usize,
}

fn package_name(spec: &str) -> Option<&str> {
    if spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with('\0')
        || spec.starts_with('#')
        || spec.contains(':')
    {
        return None;
    }
    if spec.starts_with('@') {
        let mut parts = spec.splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        Some(&spec[..scope.len() + 1 + name.len()])
    } else {
        Some(spec.split('/').next().unwrap_or(spec))
    }
}

impl LazyPackages {
    fn candidates(&self, importer: Option<&str>, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(imp) = importer
            && let Ok(rel) = Path::new(imp).strip_prefix(&self.root)
        {
            let rel = rel.to_string_lossy();
            let mut base = rel.as_ref();
            while let Some(idx) = base.rfind("node_modules/") {
                let after = &base[idx + "node_modules/".len()..];
                let pkg_len = if after.starts_with('@') {
                    after.splitn(3, '/').take(2).map(|s| s.len()).sum::<usize>() + 1
                } else {
                    after.split('/').next().map(|s| s.len()).unwrap_or(0)
                };
                let pkg_dir = &base[..idx + "node_modules/".len() + pkg_len];
                out.push(format!("{pkg_dir}/node_modules/{name}"));
                base = &base[..idx];
            }
        }
        out.push(format!("node_modules/{name}"));
        out
    }

    fn owner(&self, file: &str) -> Option<(String, String)> {
        let rel = Path::new(file)
            .strip_prefix(&self.root)
            .ok()?
            .to_string_lossy()
            .into_owned();
        let cells = self.cells.lock().unwrap();
        let mut best: Option<&String> = None;
        for k in cells.keys() {
            if rel.starts_with(&format!("{k}/")) && best.map(|b| k.len() > b.len()).unwrap_or(true) {
                best = Some(k);
            }
        }
        let k = best?.clone();
        let inner = rel[k.len() + 1..].to_string();
        Some((k, inner))
    }

    async fn index(&self, path: &str) -> Result<Arc<PkgIndex>> {
        let pkg = self
            .by_path
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("{path} is not in the lockfile"))?;
        let cell = {
            let mut cells = self.cells.lock().unwrap();
            cells
                .entry(path.to_string())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let idx = cell
            .get_or_try_init(|| async {
                let (url, integrity) = match &pkg.source {
                    acropolis_npm::Source::Registry { url, integrity } => (url.clone(), integrity.clone()),
                    acropolis_npm::Source::Git { url } => (
                        acropolis_npm::install::git_tarball_url(url).ok_or_else(|| anyhow!("git dependency {path} ({url}) is only supported for GitHub repositories pinned to a commit"))?,
                        None,
                    ),
                    other => bail!("cannot lazily fetch {path}: {other:?}"),
                };
                let blob = self.fetcher.blob(&pkg.name, &url, integrity).await?;
                let t0 = Instant::now();
                let blob_path = blob.path.clone();
                let entries = tokio::task::spawn_blocking(move || acropolis_npm::install::read_tarball(&blob_path)).await??;
                let idx = Arc::new(PkgIndex::new(entries));
                idx.materialize(&self.inside, &self.root.join(path), "package.json")?;
                let mut s = self.stats.lock().unwrap();
                s.extract_ms += t0.elapsed().as_millis() as u64;
                s.last_fetch_ms = s.last_fetch_ms.max(self.started.elapsed().as_millis() as u64);
                s.packages_fetched += 1;
                s.bytes_fetched += blob.size;
                Ok::<Arc<PkgIndex>, anyhow::Error>(idx)
            })
            .await?;
        Ok(idx.clone())
    }

    fn full_extract(&self, path: &str, idx: &PkgIndex) -> Result<()> {
        if !self.full.lock().unwrap().insert(path.to_string()) {
            return Ok(());
        }
        let root = self.root.join(path);
        for rel in idx.files.keys() {
            idx.materialize(&self.inside, &root, rel)?;
        }
        self.stats.lock().unwrap().full_extractions += 1;
        Ok(())
    }

    fn write(&self, path: &str, idx: &PkgIndex, rel: &str) -> Result<String> {
        let root = self.root.join(path);
        idx.materialize_package_jsons(&self.inside, &root, rel)?;
        idx.materialize(&self.inside, &root, rel)?;
        self.stats.lock().unwrap().files_written += 1;
        Ok(root.join(rel).to_string_lossy().into_owned())
    }

    fn prefetch_deps(self: &Arc<Self>, path: &str, idx: &PkgIndex) {
        let importer = self.root.join(path).join("package.json").to_string_lossy().into_owned();
        for key in ["dependencies", "peerDependencies"] {
            let Some(deps) = idx.pj.get(key).and_then(|d| d.as_object()) else {
                continue;
            };
            for name in deps.keys() {
                let Some(cand) = self
                    .candidates(Some(&importer), name)
                    .into_iter()
                    .find(|c| self.by_path.contains_key(c))
                else {
                    continue;
                };
                if self.cells.lock().unwrap().contains_key(&cand) {
                    continue;
                }
                let me = self.clone();
                tokio::spawn(async move {
                    if let Ok(i) = me.index(&cand).await {
                        me.prefetch_deps(&cand, &i);
                    }
                });
            }
        }
    }
}

struct LazyInstallPlugin {
    lazy: Arc<LazyPackages>,
    css: Arc<Mutex<HashMap<String, String>>>,
    base: String,
    out_dir: PathBuf,
}

const ASSET_EXTS: &[&str] = &[
    "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "bmp", "woff", "woff2", "ttf", "otf", "eot", "mp4",
    "webm", "ogg", "mp3", "wav", "flac", "aac", "pdf", "txt",
];
const INLINE_LIMIT: usize = 4096;

fn mime_of(ext: &str) -> &'static str {
    match ext {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

const PUBLIC_PREFIX: &str = "\0acropolis-public:";

impl std::fmt::Debug for LazyInstallPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LazyInstallPlugin")
    }
}

impl Plugin for LazyInstallPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("acropolis:lazy-install")
    }

    async fn resolve_id(&self, _ctx: &PluginContext, args: &HookResolveIdArgs<'_>) -> HookResolveIdReturn {
        if args.specifier.starts_with('/') && !args.specifier.starts_with("//") {
            let clean = args.specifier.split(['?', '#']).next().unwrap_or(args.specifier);
            let rel = clean.trim_start_matches('/');
            if self.lazy.root.join("public").join(rel).is_file() {
                return Ok(Some(HookResolveIdOutput::from_id(format!(
                    "{PUBLIC_PREFIX}{clean}#public"
                ))));
            }
            let p = self.lazy.root.join(rel);
            if p.is_file() {
                return Ok(Some(HookResolveIdOutput::from_id(p.to_string_lossy().into_owned())));
            }
            return Ok(None);
        }
        let require = matches!(args.kind, rolldown_common::ImportKind::Require);
        let conditions: &[&str] = if require {
            &["require", "module", "browser", "production", "default"]
        } else {
            &["import", "module", "browser", "production", "default"]
        };
        let lazy = &self.lazy;
        let (query, spec) = match args.specifier.find('?') {
            Some(i) => (&args.specifier[i..], &args.specifier[..i]),
            None => ("", args.specifier),
        };
        let found: Option<(String, Arc<PkgIndex>, Option<String>)> = if let Some(name) = package_name(spec) {
            let Some(cand) = lazy
                .candidates(args.importer, name)
                .into_iter()
                .find(|c| lazy.by_path.contains_key(c))
            else {
                return Ok(None);
            };
            let idx = lazy.index(&cand).await?;
            lazy.prefetch_deps(&cand, &idx);
            let subpath = spec[name.len()..].trim_start_matches('/');
            let rel = if idx.browser_remaps() {
                None
            } else {
                idx.resolve_subpath(subpath, conditions)
            };
            Some((cand, idx, rel))
        } else if let Some(imp) = args.importer
            && let Some((pkg, inner)) = lazy.owner(imp)
        {
            let idx = lazy.index(&pkg).await?;
            if spec.starts_with('#') {
                let rel = idx
                    .pj
                    .get("imports")
                    .and_then(|m| pkgindex::resolve_exports(m, spec, conditions))
                    .and_then(|t| idx.resolve_path(t.trim_start_matches("./")));
                Some((pkg, idx, rel))
            } else if spec.starts_with('.') {
                let dir = Path::new(&inner)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let target = if dir.is_empty() {
                    spec.to_string()
                } else {
                    format!("{dir}/{spec}")
                };
                let rel = if idx.browser_remaps() {
                    None
                } else {
                    idx.resolve_path(&target)
                };
                Some((pkg, idx, rel))
            } else {
                None
            }
        } else {
            None
        };
        let Some((pkg, idx, rel)) = found else { return Ok(None) };
        match rel {
            Some(rel) => {
                let id = lazy.write(&pkg, &idx, &rel)?;
                let mut out = HookResolveIdOutput::from_id(format!("{id}{query}"));
                out.package_json_path = Some(lazy.root.join(&pkg).join("package.json").to_string_lossy().into_owned());
                if let Some(se) = idx.side_effects(&rel) {
                    out.side_effects = Some(if se {
                        HookSideEffects::True
                    } else {
                        HookSideEffects::False
                    });
                }
                Ok(Some(out))
            }
            None => {
                lazy.full_extract(&pkg, &idx)?;
                Ok(None)
            }
        }
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        if let Some(url) = args.id.strip_prefix(PUBLIC_PREFIX) {
            let url = url.trim_end_matches("#public");
            let base = self.base.trim_end_matches('/');
            return Ok(Some(HookLoadOutput {
                code: format!("export default {:?};", format!("{base}{url}")).into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }));
        }
        let path = args.id.split('?').next().unwrap_or(args.id);
        if !args.id.starts_with('\0') && Path::new(path).is_absolute() && !self.lazy.inside.contains(Path::new(path)) {
            bail!("refusing to bundle {path}: it is missing or resolves outside the app directory");
        }
        let ext = Path::new(path)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if ASSET_EXTS.contains(&ext.as_str()) && !args.id.starts_with('\0') {
            let data = std::fs::read(path).with_context(|| format!("reading asset {path}"))?;
            let url = if data.len() < INLINE_LIMIT
                && !matches!(ext.as_str(), "woff" | "woff2" | "ttf" | "otf" | "eot")
                && !args.id.contains("?url")
            {
                use base64::Engine;
                format!(
                    "data:{};base64,{}",
                    mime_of(&ext),
                    base64::engine::general_purpose::STANDARD.encode(&data)
                )
            } else {
                let stem = Path::new(path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let name = format!("assets/{stem}-{}.{ext}", short_hash(&data));
                std::fs::create_dir_all(self.out_dir.join("assets"))?;
                std::fs::write(self.out_dir.join(&name), &data)?;
                format!("{}/{name}", self.base.trim_end_matches('/'))
            };
            return Ok(Some(HookLoadOutput {
                code: format!("export default {url:?};").into(),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }));
        }
        if path.ends_with(".css") {
            if path.ends_with(".module.css") || args.id.contains('?') {
                anyhow::bail!(
                    "CSS modules and CSS query imports are not supported by the native bundler: {}",
                    args.id
                );
            }
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
            self.css.lock().unwrap().insert(args.id.to_string(), text);
            return Ok(Some(HookLoadOutput {
                code: "export {};".into(),
                side_effects: Some(HookSideEffects::NoTreeshake),
                module_type: Some(ModuleType::Js),
                ..Default::default()
            }));
        }
        Ok(None)
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }
}

pub struct SpaInput {
    pub root: PathBuf,
    pub out_dir: PathBuf,
    pub plan: Arc<InstallPlan>,
    pub fetcher: Fetcher,
    pub base: String,
    pub env: BTreeMap<String, String>,
}

pub struct SpaOutput {
    pub stats: Stats,
    pub files: usize,
    pub ms: u64,
}

fn html_entries(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<script") {
        let start = pos + i;
        let end = match lower[start..].find('>') {
            Some(e) => start + e,
            None => break,
        };
        let tag = &html[start..=end];
        let tag_lower = &lower[start..=end];
        if (tag_lower.contains("type=\"module\"") || tag_lower.contains("type='module'"))
            && let Some(src) = attr(tag, "src")
            && !src.starts_with("http")
        {
            out.push(src);
        }
        pos = end + 1;
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let i = lower.find(&key)?;
    let rest = &tag[i + key.len()..];
    let q = rest.chars().next()?;
    if q == '"' || q == '\'' {
        let r = &rest[1..];
        let e = r.find(q)?;
        Some(r[..e].to_string())
    } else {
        Some(rest.split([' ', '>']).next()?.to_string())
    }
}

fn remove_entry_scripts(html: &str, entries: &[String]) -> String {
    let mut out = html.to_string();
    for e in entries {
        let lower = out.to_ascii_lowercase();
        let mut pos = 0;
        while let Some(i) = lower[pos..].find("<script") {
            let start = pos + i;
            let Some(close) = lower[start..].find("</script>") else {
                break;
            };
            let end = start + close + "</script>".len();
            let tag_end = start + lower[start..].find('>').unwrap_or(0);
            let tag = &out[start..=tag_end];
            if attr(tag, "src").as_deref() == Some(e.as_str()) {
                let mut s = String::new();
                s.push_str(&out[..start]);
                s.push_str(&out[end..]);
                out = s;
                break;
            }
            pos = end;
        }
    }
    out
}

pub async fn build_spa(input: SpaInput) -> Result<SpaOutput> {
    let start = Instant::now();
    let html_path = input.root.join("index.html");
    let html = std::fs::read_to_string(&html_path).context("reading index.html")?;
    let entries = html_entries(&html);
    if entries.is_empty() {
        bail!("index.html has no module script entry");
    }
    input.plan.check_paths()?;
    let mut by_path = HashMap::new();
    for p in &input.plan.packages {
        by_path.insert(p.path.clone(), p.clone());
    }
    let lazy = Arc::new(LazyPackages {
        started: Instant::now(),
        root: input.root.clone(),
        inside: Inside::new(&input.root)?,
        by_path,
        fetcher: input.fetcher.clone(),
        cells: Mutex::new(HashMap::new()),
        full: Mutex::new(std::collections::HashSet::new()),
        stats: Mutex::new(Stats {
            packages_in_lockfile: input.plan.packages.len(),
            ..Default::default()
        }),
    });
    for l in &input.plan.links {
        let dest = input.root.join(&l.path);
        if let Some(parent) = dest.parent() {
            lazy.inside.dir(parent)?;
        }
        let _ = std::fs::remove_file(&dest);
        let _ = std::os::unix::fs::symlink(&l.target, &dest);
    }
    let mut defines: Vec<(String, String)> = vec![
        ("process.env.NODE_ENV".into(), "\"production\"".into()),
        ("import.meta.env.MODE".into(), "\"production\"".into()),
        ("import.meta.env.PROD".into(), "true".into()),
        ("import.meta.env.DEV".into(), "false".into()),
        ("import.meta.env.SSR".into(), "false".into()),
        ("import.meta.env.BASE_URL".into(), format!("{:?}", input.base)),
    ];
    let mut env_obj = vec![
        ("MODE".to_string(), "\"production\"".to_string()),
        ("PROD".to_string(), "true".to_string()),
        ("DEV".to_string(), "false".to_string()),
        ("SSR".to_string(), "false".to_string()),
        ("BASE_URL".to_string(), format!("{:?}", input.base)),
    ];
    for (k, v) in &input.env {
        if k.starts_with("VITE_") {
            defines.push((format!("import.meta.env.{k}"), format!("{v:?}")));
            env_obj.push((k.clone(), format!("{v:?}")));
        }
    }
    let obj = format!(
        "{{{}}}",
        env_obj
            .iter()
            .map(|(k, v)| format!("{k:?}:{v}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    defines.push(("import.meta.env".into(), obj));
    let options = BundlerOptions {
        input: Some(
            entries
                .iter()
                .map(|e| InputItem {
                    name: Some("index".into()),
                    import: format!("./{}", e.trim_start_matches('/')),
                })
                .collect(),
        ),
        cwd: Some(input.root.clone()),
        dir: Some(input.out_dir.to_string_lossy().into_owned()),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Browser),
        minify: Some(RawMinifyOptions::Bool(true)),
        entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        asset_filenames: Some("assets/[name]-[hash][extname]".to_string().into()),
        define: Some(defines.into_iter().collect()),
        ..Default::default()
    };
    let css_map: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let plugin = rolldown::plugin::__inner::Pluginable::new_shared(LazyInstallPlugin {
        lazy: lazy.clone(),
        css: css_map.clone(),
        base: input.base.clone(),
        out_dir: input.out_dir.clone(),
    });
    let mut bundler = Bundler::with_plugins(options, vec![plugin]).map_err(|e| anyhow!("{e:?}"))?;
    let output = bundler.write().await.map_err(|e| anyhow!("bundling failed: {e:?}"))?;
    let mut entry_js = None;
    let mut css = Vec::new();
    let mut chunks: Vec<&Arc<rolldown_common::OutputChunk>> = Vec::new();
    for o in &output.assets {
        match o {
            rolldown_common::Output::Chunk(c) => {
                if c.is_entry {
                    entry_js = Some(c.filename.to_string());
                }
                chunks.push(c);
            }
            rolldown_common::Output::Asset(a) if a.filename.ends_with(".css") => css.push(a.filename.to_string()),
            _ => {}
        }
    }
    chunks.sort_by_key(|c| (!c.is_entry, c.filename.to_string()));
    let css_contents = css_map.lock().unwrap().clone();
    let mut ordered: Vec<String> = Vec::new();
    for c in &chunks {
        for m in &c.module_ids {
            let id = m.as_str().to_string();
            if css_contents.contains_key(&id) && !ordered.contains(&id) {
                ordered.push(id);
            }
        }
    }
    for id in css_contents.keys() {
        if !ordered.contains(id) {
            ordered.push(id.clone());
        }
    }
    ordered.dedup();
    if !ordered.is_empty() {
        let mut combined = String::new();
        let mut copied: HashMap<PathBuf, String> = HashMap::new();
        for id in &ordered {
            let path = PathBuf::from(id.split('?').next().unwrap_or(id));
            combined.push_str(&process_css(
                &lazy.inside,
                &path,
                &css_contents[id],
                &input.out_dir,
                &input.base,
                &mut copied,
                0,
            )?);
        }
        let hash = short_hash(combined.as_bytes());
        let name = format!("assets/index-{hash}.css");
        std::fs::create_dir_all(input.out_dir.join("assets"))?;
        std::fs::write(input.out_dir.join(&name), &combined)?;
        css.push(name);
    }
    let entry_js = entry_js.ok_or_else(|| anyhow!("bundle produced no entry chunk"))?;
    let base = input.base.trim_end_matches('/');
    let mut head = format!("<script type=\"module\" crossorigin src=\"{base}/{entry_js}\"></script>");
    for c in &css {
        head.push_str(&format!(
            "\n    <link rel=\"stylesheet\" crossorigin href=\"{base}/{c}\">"
        ));
    }
    let mut out_html = remove_entry_scripts(&html, &entries);
    match out_html.to_ascii_lowercase().find("</head>") {
        Some(i) => out_html.insert_str(i, &format!("  {head}\n  ")),
        None => out_html = format!("{head}\n{out_html}"),
    }
    std::fs::write(input.out_dir.join("index.html"), out_html)?;
    let public = input.root.join("public");
    if public.is_dir() {
        copy_dir(&lazy.inside, &public, &input.out_dir, 0)?;
    }
    let stats = lazy.stats.lock().unwrap().clone();
    Ok(SpaOutput {
        stats,
        files: output.assets.len() + 1,
        ms: start.elapsed().as_millis() as u64,
    })
}

fn short_hash(data: &[u8]) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(data);
    let mut s = String::new();
    for b in h.iter().take(5) {
        s.push_str(&format!("{b:02x}"));
    }
    s.truncate(8);
    s
}

fn process_css(
    inside: &Inside,
    path: &Path,
    text: &str,
    out_dir: &Path,
    base: &str,
    copied: &mut HashMap<PathBuf, String>,
    depth: usize,
) -> Result<String> {
    use lightningcss::dependencies::{Dependency, DependencyOptions};
    use lightningcss::printer::PrinterOptions;
    use lightningcss::stylesheet::{MinifyOptions, ParserOptions, StyleSheet};
    if depth > 32 {
        bail!("@import nesting too deep at {}", path.display());
    }
    let filename = path.to_string_lossy().into_owned();
    let mut sheet = StyleSheet::parse(
        text,
        ParserOptions {
            filename: filename.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| anyhow!("parsing {filename}: {e}"))?;
    sheet
        .minify(MinifyOptions::default())
        .map_err(|e| anyhow!("minifying {filename}: {e}"))?;
    let res = sheet
        .to_css(PrinterOptions {
            minify: true,
            analyze_dependencies: Some(DependencyOptions { remove_imports: true }),
            ..Default::default()
        })
        .map_err(|e| anyhow!("printing {filename}: {e}"))?;
    let mut code = res.code;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut prefix = String::new();
    for dep in res.dependencies.unwrap_or_default() {
        match dep {
            Dependency::Import(imp) => {
                if imp.url.starts_with("http") || imp.url.starts_with("//") {
                    prefix.push_str(&format!("@import url({:?});", imp.url));
                    continue;
                }
                let target = dir.join(&imp.url);
                if !inside.contains(&target) {
                    bail!(
                        "@import {} from {filename} is missing or resolves outside the app directory",
                        imp.url
                    );
                }
                let t =
                    std::fs::read_to_string(&target).with_context(|| format!("@import {} from {filename}", imp.url))?;
                prefix.push_str(&process_css(inside, &target, &t, out_dir, base, copied, depth + 1)?);
            }
            Dependency::Url(u) => {
                let url = u.url.clone();
                let replacement = if url.starts_with("data:")
                    || url.starts_with("http")
                    || url.starts_with("//")
                    || url.starts_with('#')
                    || url.starts_with('/')
                {
                    url.clone()
                } else {
                    let clean = url.split(['?', '#']).next().unwrap_or(&url);
                    let src = dir.join(clean);
                    match copied.get(&src) {
                        Some(r) => r.clone(),
                        None => {
                            if !inside.contains(&src) {
                                bail!("url({url}) in {filename} is missing or resolves outside the app directory");
                            }
                            let data = std::fs::read(&src).with_context(|| format!("url({url}) in {filename}"))?;
                            let stem = src
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let ext = src
                                .extension()
                                .map(|s| format!(".{}", s.to_string_lossy()))
                                .unwrap_or_default();
                            let name = format!("assets/{stem}-{}{ext}", short_hash(&data));
                            std::fs::create_dir_all(out_dir.join("assets"))?;
                            std::fs::write(out_dir.join(&name), &data)?;
                            let r = format!("{}/{name}", base.trim_end_matches('/'));
                            copied.insert(src.clone(), r.clone());
                            r
                        }
                    }
                };
                code = code.replace(&u.placeholder, &replacement);
            }
        }
    }
    Ok(format!("{prefix}{code}"))
}

fn copy_dir(inside: &Inside, from: &Path, to: &Path, depth: usize) -> Result<()> {
    if depth > 32 {
        bail!("{} is nested too deep (symlink loop?)", from.display());
    }
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let p = e.path();
        if !inside.contains(&p) {
            bail!("{} is a broken link or points outside the app directory", p.display());
        }
        let t = to.join(e.file_name());
        if p.is_dir() {
            copy_dir(inside, &p, &t, depth + 1)?;
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

pub fn simple_vite_config(root: &Path) -> bool {
    let cfg = ["vite.config.ts", "vite.config.js", "vite.config.mjs", "vite.config.mts"]
        .iter()
        .find_map(|f| std::fs::read_to_string(root.join(f)).ok());
    let Some(cfg) = cfg else { return true };
    let mut imports: Vec<String> = Vec::new();
    for line in cfg.lines() {
        let l = line.trim();
        if l.starts_with("import ")
            && let Some(from) = l.rsplit(" from ").next()
        {
            imports.push(
                from.trim()
                    .trim_end_matches(';')
                    .trim_matches(|c| c == '\'' || c == '"')
                    .to_string(),
            );
        }
    }
    let allowed = [
        "vite",
        "@vitejs/plugin-react",
        "@vitejs/plugin-react-swc",
        "node:path",
        "path",
        "node:url",
        "url",
    ];
    let postcss = [
        "postcss.config.js",
        "postcss.config.cjs",
        "postcss.config.mjs",
        "postcss.config.ts",
        "tailwind.config.js",
        "tailwind.config.ts",
    ]
    .iter()
    .any(|f| root.join(f).exists());
    !postcss
        && imports.iter().all(|i| allowed.contains(&i.as_str()))
        && !cfg.contains("resolve:")
        && !cfg.contains("css:")
        && !cfg.contains("build:")
        && !cfg.contains("define:")
        && !cfg.contains("base:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_and_names() {
        let html = r#"<html><head><script type="module" src="/src/main.tsx"></script></head><body></body></html>"#;
        assert_eq!(html_entries(html), vec!["/src/main.tsx"]);
        assert_eq!(package_name("@mui/material/Button"), Some("@mui/material"));
        assert_eq!(package_name("react-dom/client"), Some("react-dom"));
        assert_eq!(package_name("./x"), None);
        let out = remove_entry_scripts(html, &["/src/main.tsx".to_string()]);
        assert!(!out.contains("main.tsx"));
    }

    #[test]
    fn css_and_public_reads_stay_inside_root() {
        let base = std::env::temp_dir().join(format!("acropolis-bundle-inside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("app");
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(base.join("secret.png"), "s").unwrap();
        std::os::unix::fs::symlink(base.join("secret.png"), root.join("public/x.png")).unwrap();
        let inside = Inside::new(&root).unwrap();
        let out = root.join("dist");
        assert!(copy_dir(&inside, &root.join("public"), &out, 0).is_err());
        assert!(!out.join("x.png").exists());
        let mut copied = HashMap::new();
        assert!(
            process_css(
                &inside,
                &root.join("a.css"),
                "a{background:url(../secret.png)}",
                &out,
                "/",
                &mut copied,
                0
            )
            .is_err()
        );
        assert!(
            process_css(
                &inside,
                &root.join("a.css"),
                "@import '../secret.png';",
                &out,
                "/",
                &mut copied,
                0
            )
            .is_err()
        );
        std::fs::remove_file(root.join("public/x.png")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("public/loop")).unwrap();
        assert!(copy_dir(&inside, &root.join("public"), &out, 0).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
