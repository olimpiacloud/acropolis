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
        #[arg(long, default_value_t = 64)]
        concurrency: usize,
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
        #[arg(long, env = "ACROPOLIS_CACHE_KEY")]
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
    Report {
        results: Vec<PathBuf>,
    },
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
            .with_context(|| format!("invalid env {p:?}, expected KEY=VALUE"))?;
        env.vars.insert(k.to_string(), v.to_string());
    }
    Ok(env)
}

fn mirrors(list: &[String]) -> Result<Vec<(String, String)>> {
    list.iter()
        .filter(|m| !m.is_empty())
        .map(|m| {
            m.split_once('=')
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .with_context(|| format!("invalid mirror {m:?}, expected REGISTRY=MIRROR"))
        })
        .collect()
}

fn default_home() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(x).join("acropolis");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
        .join(".cache")
        .join("acropolis")
}

fn main() {
    let cli = Cli::parse();
    if let Command::Bench {
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
    } = &cli.cmd
    {
        let repo = std::fs::canonicalize(repo).expect("repo path");
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
            acropolis_bin: std::env::current_exe().expect("current exe"),
            railpack_bin: railpack.clone(),
            railpack_frontend: railpack_frontend.clone(),
            compression: compression.clone(),
            drop_caches: !no_drop_caches,
            scenario: scenario.clone(),
            cache,
            fresh: *fresh,
            apps_file: apps_file.clone(),
        };
        match bench::run(cfg) {
            Ok(results) => {
                println!("{}", bench::summarize(&results));
                eprintln!("results: {}", out.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
    }
    if let Command::E2e {
        examples,
        filter,
        jobs,
        registry,
        out,
        isolated,
        failed_from,
    } = &cli.cmd
    {
        let only = match failed_from {
            Some(p) => match e2e::failed_cases(p) {
                Ok(set) => Some(set),
                Err(e) => {
                    eprintln!("error: {e:#}");
                    std::process::exit(2);
                }
            },
            None => None,
        };
        let out = out
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("e2e-{}.jsonl", std::process::id())));
        let home = cli
            .home
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("acropolis-e2e-home"));
        let cfg = e2e::E2eConfig {
            examples: std::fs::canonicalize(examples).expect("examples dir"),
            filter: filter.clone(),
            jobs: *jobs,
            acropolis_bin: std::env::current_exe().expect("current exe"),
            registry: registry.clone(),
            out: out.clone(),
            home,
            isolated: *isolated,
            only,
        };
        match e2e::run(cfg) {
            Ok(results) => {
                println!("{}", e2e::summary(&results));
                eprintln!("results: {}", out.display());
                std::process::exit(if results.iter().any(|r| r.status == "fail") {
                    1
                } else {
                    0
                });
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(2);
            }
        }
    }
    if let Command::Report { results } = &cli.cmd {
        let mut all = Vec::new();
        for p in results {
            let text = std::fs::read_to_string(p).expect("results file");
            let mut rs: Vec<bench::RunResult> = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| serde_json::from_str::<bench::RunResult>(line).expect("result line"))
                .collect();
            bench::backfill(&mut rs, &p.with_extension("logs"));
            for r in &mut rs {
                if r.tool == "acro" || r.tool.starts_with("acro@") {
                    r.tool = r.tool.replacen("acro", "acropolis", 1);
                }
            }
            all.extend(rs);
        }
        println!("{}", bench::summarize(&all));
        std::process::exit(0);
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(e) => {
            let class = acropolis_build::errors::classify(&e);
            acropolis_events::emit(acropolis_events::Event::BuildFailed {
                ms: acropolis_events::elapsed_ms(),
                class: class.name().to_string(),
                exit_code: class.exit_code(),
                error: format!("{e:#}"),
            });
            if acropolis_events::mode() != acropolis_events::Mode::Json {
                eprintln!("error: {e:#}");
                eprintln!("error class: {} (exit {})", class.name(), class.exit_code());
            }
            class.exit_code()
        }
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
            let compression =
                Compression::parse(&compression).with_context(|| format!("unknown compression {compression}"))?;
            let target = tag.as_deref().map(Reference::parse).transpose()?;
            let abs_home = std::path::absolute(&home)?;
            let readonly = vec![
                abs_home.join("store"),
                abs_home.join("toolchains"),
                std::env::var_os("ACROPOLIS_ROOTFS")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| abs_home.join("rootfs")),
            ];
            let exec: Arc<dyn Executor> = if hermetic == "off" {
                Arc::new(HostExecutor {
                    isolation: Isolation::None,
                    readonly: Vec::new(),
                })
            } else {
                let e = HostExecutor::detect();
                if e.isolation == Isolation::None {
                    bail!(
                        "cannot create network namespaces for hermetic build steps; rerun with --hermetic=off to build without isolation"
                    );
                }
                Arc::new(e.with_readonly(readonly))
            };
            let opts = BuildOptions {
                app_dir: std::fs::canonicalize(&dir).with_context(|| format!("{} not found", dir.display()))?,
                home,
                target,
                env: env.clone(),
                layer: LayerOptions {
                    compression,
                    level,
                    threads: 0,
                },
                mirrors,
                concurrency,
                keep_work,
                platform: acropolis_oci::image::host_platform(),
                oci_out: oci.map(|p| std::path::absolute(&p)).transpose()?,
            };
            let built = acropolis_build::build(opts, exec).await;
            if let Some(path) = &info {
                let plan = acropolis_build::plan_app(&std::fs::canonicalize(&dir)?, &env).ok();
                std::fs::write(
                    path,
                    serde_json::to_vec_pretty(&build_info(
                        plan.as_ref(),
                        built.as_ref().ok().map(|(_, r)| r),
                        built.as_ref().err(),
                    ))?,
                )?;
            }
            let (_plan, res) = built?;
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
            let client = reqwest::Client::builder().build()?;
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
