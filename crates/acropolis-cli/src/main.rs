mod bench;
mod e2e;

use acropolis_build::{BuildOptions, Env};
use acropolis_exec::{Executor, HostExecutor, Isolation};
use acropolis_oci::{Compression, LayerOptions, Reference};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "acropolis",
    version,
    about = "Acropolis: fast, daemonless, reproducible app builder"
)]
struct Cli {
    #[arg(long, global = true, default_value = "human", value_parser = ["human", "json", "quiet"])]
    events: String,
    #[arg(long, global = true, env = "ACROPOLIS_HOME")]
    home: Option<PathBuf>,
    #[arg(
        long = "mirror",
        global = true,
        value_name = "REGISTRY=MIRROR",
        env = "ACROPOLIS_MIRRORS",
        value_delimiter = ','
    )]
    mirrors: Vec<String>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    Build {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(short, long)]
        tag: Option<String>,
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long, default_value = "zstd")]
        compression: String,
        #[arg(long, default_value_t = 0)]
        level: i32,
        #[arg(long, default_value = "on", value_parser = ["on", "off"])]
        hermetic: String,
        #[arg(long)]
        keep_work: bool,
        #[arg(long, default_value = "64")]
        concurrency: std::num::NonZeroUsize,
        #[arg(long)]
        config: Option<String>,
        #[arg(long, value_name = "FILE")]
        oci: Option<PathBuf>,
        #[arg(long, value_name = "FILE")]
        info: Option<PathBuf>,
    },
    Plan {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Inspect {
        reference: String,
    },
    Gc {
        #[arg(long)]
        max_size: String,
    },
    Cache {
        #[arg(value_parser = ["export", "import"])]
        action: String,
        file: PathBuf,
        #[arg(long, env = "ACROPOLIS_CACHE_KEY", value_parser = clap::builder::NonEmptyStringValueParser::new())]
        key: String,
    },
    Prewarm {
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "node:lts,node:22,node:24,go:latest,bun:latest,uv:latest"
        )]
        tools: Vec<String>,
    },
    #[command(hide = true)]
    Bench {
        #[arg(long, value_delimiter = ',', default_value = "express-api,go-api")]
        apps: Vec<String>,
        #[arg(long, value_delimiter = ',', default_value = "docker,railpack,acropolis")]
        tools: Vec<String>,
        #[arg(long, default_value_t = 2)]
        runs: usize,
        #[arg(long, default_value = "0-1")]
        cpus: String,
        #[arg(long)]
        memory: Option<String>,
        #[arg(long, default_value = "on", value_parser = ["on", "off"])]
        mirror: String,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "railpack")]
        railpack: PathBuf,
        #[arg(long, default_value = "ghcr.io/railwayapp/railpack-frontend:v0.40.1")]
        railpack_frontend: String,
        #[arg(long, default_value = "zstd")]
        compression: String,
        #[arg(long)]
        no_drop_caches: bool,
        #[arg(long, default_value = "both", value_parser = ["cold", "rebuild", "both"])]
        scenario: String,
        #[arg(long)]
        cache: Option<PathBuf>,
        #[arg(long)]
        fresh: bool,
        #[arg(long)]
        apps_file: Option<PathBuf>,
    },
    #[command(hide = true)]
    Report {
        results: Vec<PathBuf>,
    },
    #[command(hide = true)]
    E2e {
        #[arg(long)]
        examples: PathBuf,
        #[arg(long, value_delimiter = ',')]
        filter: Vec<String>,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        #[arg(long, default_value = "localhost:5001")]
        registry: String,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        isolated: bool,
        #[arg(long)]
        failed_from: Option<PathBuf>,
    },
}

fn build_info(
    plan: Option<&acropolis_build::Plan>,
    res: Option<&acropolis_build::BuildResult>,
    err: Option<&anyhow::Error>,
) -> serde_json::Value {
    let mut packages = serde_json::Map::new();
    let mut requested: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    if let Some(p) = plan {
        for s in &p.steps {
            if let acropolis_build::plan::Action::Toolchain { tool, spec, .. } = &s.action {
                requested.insert(tool.clone(), spec.clone());
            }
        }
    }
    if let Some(r) = res {
        for (name, version) in &r.tools {
            let display = match name.as_str() {
                "python-standalone" => "python",
                other => other.strip_prefix("npm:").unwrap_or(other),
            };
            let mut pkg = serde_json::json!({ "name": display, "resolvedVersion": version, "source": "acropolis" });
            if let Some(spec) = requested.get(name).filter(|s| !s.is_empty()) {
                pkg["requestedVersion"] = serde_json::json!(spec);
            }
            packages.insert(display.to_string(), pkg);
        }
    }
    let mut metadata = serde_json::Map::new();
    if let Some(p) = plan {
        for (k, v) in &p.facts {
            metadata.insert(k.clone(), serde_json::json!(v));
        }
        if let Some(pm) = p.facts.get("package-manager") {
            metadata.insert("nodePackageManager".into(), serde_json::json!(pm));
        }
    }
    serde_json::json!({
        "acropolisVersion": env!("CARGO_PKG_VERSION"),
        "detectedProviders": plan.map(|p| vec![p.provider.clone()]).unwrap_or_default(),
        "resolvedPackages": packages,
        "metadata": metadata,
        "planHash": plan.map(|p| p.hash.clone()),
        "manifestDigest": res.map(|r| r.manifest_digest.clone()),
        "warnings": plan.map(|p| p.warnings.clone()).unwrap_or_default(),
        "success": res.is_some(),
        "error": err.map(|e| format!("{e:#}")),
    })
}

fn parse_env(pairs: &[String]) -> Result<Env> {
    let mut env = Env::default();
    for p in pairs {
        let (k, v) = p
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .with_context(|| format!("invalid env {p:?}, expected KEY=VALUE"))?;
        env.vars.insert(k.to_string(), v.to_string());
    }
    Ok(env)
}

fn mirrors(list: &[String]) -> Result<Vec<(String, String)>> {
    list.iter()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(|m| {
            m.split_once('=')
                .map(|(a, b)| (a.trim(), b.trim()))
                .filter(|(a, b)| !a.is_empty() && !b.is_empty())
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .with_context(|| format!("invalid mirror {m:?}, expected REGISTRY=MIRROR"))
        })
        .collect()
}

fn default_home() -> PathBuf {
    // Empty or relative values are invalid per the XDG spec; using them would put the store under the current directory.
    let abs = |k: &str| std::env::var_os(k).map(PathBuf::from).filter(|p| p.is_absolute());
    match abs("XDG_CACHE_HOME") {
        Some(x) => x.join("acropolis"),
        None => abs("HOME")
            .unwrap_or_else(|| "/tmp".into())
            .join(".cache")
            .join("acropolis"),
    }
}

fn dev_command(cli: &Cli) -> Option<Result<i32>> {
    let res = match &cli.cmd {
        Command::Bench {
            apps,
            tools,
            runs,
            cpus,
            memory,
            mirror,
            out,
            repo,
            railpack,
            railpack_frontend,
            compression,
            no_drop_caches,
            scenario,
            cache,
            fresh,
            apps_file,
        } => (|| {
            let repo = std::fs::canonicalize(repo).with_context(|| format!("repo {}", repo.display()))?;
            let cache = Some(
                cache
                    .clone()
                    .unwrap_or_else(|| repo.join("bench/results/competitors.jsonl")),
            );
            let out = out.clone().unwrap_or_else(|| {
                repo.join("bench/results")
                    .join(format!("run-{}.jsonl", std::process::id()))
            });
            let cfg = bench::BenchConfig {
                repo,
                apps: apps.clone(),
                tools: tools.clone(),
                runs: *runs,
                cpus: cpus.clone(),
                memory: memory.clone(),
                mirror: mirror == "on",
                out: out.clone(),
                acropolis_bin: std::env::current_exe()?,
                railpack_bin: railpack.clone(),
                railpack_frontend: railpack_frontend.clone(),
                compression: compression.clone(),
                drop_caches: !no_drop_caches,
                scenario: scenario.clone(),
                cache,
                fresh: *fresh,
                apps_file: apps_file.clone(),
            };
            let results = bench::run(cfg)?;
            println!("{}", bench::summarize(&results));
            eprintln!("results: {}", out.display());
            Ok(0)
        })(),
        Command::E2e {
            examples,
            filter,
            jobs,
            registry,
            out,
            isolated,
            failed_from,
        } => (|| {
            let only = failed_from.as_deref().map(e2e::failed_cases).transpose()?;
            let out = out
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("e2e-{}.jsonl", std::process::id())));
            let cfg = e2e::E2eConfig {
                examples: std::fs::canonicalize(examples)
                    .with_context(|| format!("examples {}", examples.display()))?,
                filter: filter.clone(),
                jobs: *jobs,
                acropolis_bin: std::env::current_exe()?,
                registry: registry.clone(),
                out: out.clone(),
                home: cli
                    .home
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("acropolis-e2e-home")),
                isolated: *isolated,
                only,
            };
            let results = e2e::run(cfg)?;
            println!("{}", e2e::summary(&results));
            eprintln!("results: {}", out.display());
            Ok(if results.iter().any(|r| r.status == "fail") {
                1
            } else {
                0
            })
        })(),
        Command::Report { results } => (|| {
            let mut all = Vec::new();
            for p in results {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let mut rs = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(serde_json::from_str::<bench::RunResult>)
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(|| format!("parsing {}", p.display()))?;
                bench::backfill(&mut rs, &p.with_extension("logs"));
                for r in &mut rs {
                    if r.tool == "acro" || r.tool.starts_with("acro@") {
                        r.tool = r.tool.replacen("acro", "acropolis", 1);
                    }
                }
                all.extend(rs);
            }
            println!("{}", bench::summarize(&all));
            Ok(0)
        })(),
        _ => return None,
    };
    Some(res)
}

fn fail(class: acropolis_build::errors::ErrorClass, error: String) -> i32 {
    if acropolis_events::mode() != acropolis_events::Mode::Json {
        eprintln!("error: {error}");
        eprintln!("error class: {} (exit {})", class.name(), class.exit_code());
    }
    acropolis_events::emit(acropolis_events::Event::BuildFailed {
        ms: acropolis_events::elapsed_ms(),
        class: class.name().to_string(),
        exit_code: class.exit_code(),
        error,
    });
    class.exit_code()
}

fn main() {
    let cli = Cli::parse();
    if let Some(res) = dev_command(&cli) {
        std::process::exit(res.unwrap_or_else(|e| {
            eprintln!("error: {e:#}");
            2
        }));
    }
    if let Ok(id) = std::env::var("ACROPOLIS_BUILD_ID")
        && !id.trim().is_empty()
    {
        acropolis_events::set_build_id(id.trim());
    }
    acropolis_events::init(match cli.events.as_str() {
        "json" => acropolis_events::Mode::Json,
        "quiet" => acropolis_events::Mode::Quiet,
        _ => acropolis_events::Mode::Human,
    });
    // With panic = "abort" a panic would otherwise end in SIGABRT (not the documented exit 70) and, in json mode, a non-JSON stderr line.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if acropolis_events::mode() == acropolis_events::Mode::Human {
            default_hook(info);
        }
        std::process::exit(fail(
            acropolis_build::errors::ErrorClass::Internal,
            format!("panic: {info}"),
        ));
    }));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(e) => fail(acropolis_build::errors::classify(&e), format!("{e:#}")),
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<()> {
    let home = cli.home.clone().unwrap_or_else(default_home);
    let mirrors = mirrors(&cli.mirrors)?;
    match cli.cmd {
        Command::Plan { dir, env, json } => {
            let env = parse_env(&env)?.with_operator(std::env::vars());
            let plan = acropolis_build::plan_app(&dir, &env)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print!("{}", plan.render());
            }
            Ok(())
        }
        Command::Build {
            dir,
            tag,
            env,
            compression,
            level,
            hermetic,
            keep_work,
            concurrency,
            config,
            oci,
            info,
        } => {
            let mut env = parse_env(&env)?.with_operator(std::env::vars());
            if let Some(c) = config {
                env.vars.insert("ACROPOLIS_CONFIG_FILE".into(), c);
            }
            let built = async {
                let compression = Compression::parse(&compression).with_context(|| format!("unknown compression {compression}"))?;
                if matches!(compression, Compression::Gzip) && level > 9 {
                    bail!("invalid --level {level} for gzip, expected 1-9");
                }
                let target = tag.as_deref().map(Reference::parse).transpose()?;
                let abs_home = std::path::absolute(&home)?;
                let readonly = vec![
                    abs_home.join("store"),
                    abs_home.join("toolchains"),
                    std::env::var_os("ACROPOLIS_ROOTFS").map(PathBuf::from).unwrap_or_else(|| abs_home.join("rootfs")),
                ];
                let exec: Arc<dyn Executor> = if hermetic == "off" {
                    Arc::new(HostExecutor { isolation: Isolation::None, readonly: Vec::new() })
                } else {
                    let e = HostExecutor::detect();
                    if e.isolation == Isolation::None {
                        bail!("cannot create network namespaces for hermetic build steps; rerun with --hermetic=off to build without isolation");
                    }
                    Arc::new(e.with_readonly(readonly))
                };
                let opts = BuildOptions {
                    app_dir: std::fs::canonicalize(&dir).with_context(|| format!("{} not found", dir.display()))?,
                    home,
                    target,
                    env: env.clone(),
                    layer: LayerOptions { compression, level, threads: 0 },
                    mirrors,
                    concurrency: concurrency.get(),
                    keep_work,
                    platform: acropolis_oci::image::host_platform(),
                    oci_out: oci.map(|p| std::path::absolute(&p)).transpose()?,
                };
                acropolis_build::build(opts, exec).await
            }
            .await;
            let written = match &info {
                Some(path) => {
                    let replanned = if built.is_err() {
                        std::fs::canonicalize(&dir)
                            .ok()
                            .and_then(|d| acropolis_build::plan_app(&d, &env).ok())
                    } else {
                        None
                    };
                    let plan = built.as_ref().ok().map(|(p, _)| p).or(replanned.as_ref());
                    serde_json::to_vec_pretty(&build_info(
                        plan,
                        built.as_ref().ok().map(|(_, r)| r),
                        built.as_ref().err(),
                    ))
                    .map_err(anyhow::Error::from)
                    .and_then(|json| std::fs::write(path, json).with_context(|| format!("writing {}", path.display())))
                }
                None => Ok(()),
            };
            let (_plan, res) = built?;
            written?;
            acropolis_events::emit(acropolis_events::Event::Stats(acropolis_events::stats()));
            acropolis_events::emit(acropolis_events::Event::BuildFinished {
                ms: acropolis_events::elapsed_ms(),
                image: res.reference.clone(),
                digest: Some(res.manifest_digest.clone()),
            });
            if acropolis_events::mode() != acropolis_events::Mode::Json {
                match &res.reference {
                    Some(r) => println!("{r}@{}", res.manifest_digest),
                    None => println!("{}", res.manifest_digest),
                }
            }
            Ok(())
        }
        Command::Bench { .. } | Command::Report { .. } | Command::E2e { .. } => unreachable!(),
        Command::Prewarm { tools } => {
            let mut b = acropolis_build::plan::PlanBuilder::new("prewarm", "prewarm");
            for (i, t) in tools.iter().enumerate() {
                let (tool, spec) = t.split_once(':').unwrap_or((t.as_str(), "latest"));
                let spec = if spec == "latest" && tool != "node" {
                    String::new()
                } else {
                    spec.to_string()
                };
                let parts = if tool == "node" {
                    vec!["npm".to_string(), "headers".to_string()]
                } else {
                    vec![]
                };
                let tool = if tool == "python" { "python-standalone" } else { tool };
                b.step(
                    &format!("tool-{i}"),
                    format!("{tool} {}", if spec.is_empty() { "latest" } else { &spec }),
                    acropolis_build::plan::Action::Toolchain {
                        tool: tool.to_string(),
                        spec,
                        parts,
                    },
                    &[],
                );
            }
            let plan = b.finish();
            let scratch = std::env::temp_dir().join(format!("acropolis-prewarm-{}", std::process::id()));
            std::fs::create_dir_all(&scratch)?;
            let mut env = Env::default();
            env.vars.insert("ACROPOLIS_PREWARM".into(), "1".into());
            let opts = BuildOptions {
                app_dir: scratch.clone(),
                home: home.clone(),
                target: None,
                env,
                layer: LayerOptions {
                    compression: Compression::Zstd,
                    level: 0,
                    threads: 0,
                },
                mirrors,
                concurrency: 64,
                keep_work: false,
                platform: acropolis_oci::image::host_platform(),
                oci_out: None,
            };
            let exec: Arc<dyn Executor> = Arc::new(HostExecutor {
                isolation: Isolation::None,
                readonly: Vec::new(),
            });
            let res = acropolis_build::run::execute(plan, opts, exec).await;
            let _ = std::fs::remove_dir_all(&scratch);
            res?;
            Ok(())
        }
        Command::Cache { action, file, key } => {
            let started = std::time::Instant::now();
            if action == "export" {
                let n = acropolis_build::cache::export(&home, &key, &file)?;
                println!(
                    "cache {key}: exported {:.1} MB to {} in {:.1}s",
                    n as f64 / 1e6,
                    file.display(),
                    started.elapsed().as_secs_f64()
                );
            } else {
                let n = acropolis_build::cache::import(&home, &key, &file)?;
                println!(
                    "cache {key}: imported {:.1} MB from {} in {:.1}s",
                    n as f64 / 1e6,
                    file.display(),
                    started.elapsed().as_secs_f64()
                );
            }
            Ok(())
        }
        Command::Gc { max_size } => {
            let max = acropolis_build::gc::parse_size(&max_size).with_context(|| format!("invalid size {max_size}"))?;
            let rootfs = std::env::var_os("ACROPOLIS_ROOTFS")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("rootfs"));
            let r = acropolis_build::gc::collect(&home, &rootfs, max)?;
            println!(
                "gc: {:.2} GB -> {:.2} GB, {} entries removed{}",
                r.before as f64 / 1e9,
                r.after as f64 / 1e9,
                r.removed,
                if r.exclusive {
                    ""
                } else {
                    " (builds running: only entries idle for over 1h)"
                }
            );
            Ok(())
        }
        Command::Inspect { reference } => {
            let r = Reference::parse(&reference)?;
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .read_timeout(std::time::Duration::from_secs(60))
                .build()?;
            let reg = acropolis_build::run::make_registry(client, &mirrors);
            let img = reg.resolve(&r, &acropolis_oci::image::host_platform()).await?;
            let out = serde_json::json!({
                "reference": img.reference.to_string(),
                "digest": img.manifest_digest,
                "manifest": img.manifest,
                "config": img.config,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache_key_rejected() {
        assert!(Cli::try_parse_from(["acropolis", "cache", "export", "out.tar", "--key", ""]).is_err());
        assert!(Cli::try_parse_from(["acropolis", "cache", "export", "out.tar", "--key", "k"]).is_ok());
    }

    #[test]
    fn env_pairs() {
        let env = parse_env(&["A=b=c".into(), "EMPTY=".into()]).unwrap();
        assert_eq!(env.vars["A"], "b=c");
        assert_eq!(env.vars["EMPTY"], "");
        assert!(parse_env(&["=x".into()]).is_err());
        assert!(parse_env(&["NOVALUE".into()]).is_err());
    }

    #[test]
    fn mirror_list() {
        assert_eq!(
            mirrors(&[
                "docker.io=mirror.gcr.io".into(),
                " ghcr.io = m.example ".into(),
                "".into()
            ])
            .unwrap(),
            [
                ("docker.io".to_string(), "mirror.gcr.io".to_string()),
                ("ghcr.io".into(), "m.example".into())
            ]
        );
        assert!(mirrors(&["docker.io=".into()]).is_err());
        assert!(mirrors(&["=mirror.gcr.io".into()]).is_err());
    }
}
