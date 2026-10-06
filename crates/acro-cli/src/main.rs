use acro_build::{BuildOptions, Env};
use acro_exec::{Executor, HostExecutor, Isolation};
use acro_oci::{Compression, LayerOptions, Reference};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "acro", version, about = "Acropolis: fast, daemonless, reproducible app builder")]
struct Cli {
    #[arg(long, global = true, default_value = "human", value_parser = ["human", "json", "quiet"])]
    events: String,
    #[arg(long, global = true, env = "ACRO_HOME")]
    home: Option<PathBuf>,
    #[arg(long = "mirror", global = true, value_name = "REGISTRY=MIRROR", env = "ACRO_MIRRORS", value_delimiter = ',')]
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
}

fn parse_env(pairs: &[String]) -> Result<Env> {
    let mut env = Env::default();
    for p in pairs {
        let (k, v) = p.split_once('=').with_context(|| format!("invalid env {p:?}, expected KEY=VALUE"))?;
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
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache").join("acropolis")
}

fn main() {
    let cli = Cli::parse();
    acro_events::init(match cli.events.as_str() {
        "json" => acro_events::Mode::Json,
        "quiet" => acro_events::Mode::Quiet,
        _ => acro_events::Mode::Human,
    });
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let code = match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<()> {
    let home = cli.home.clone().unwrap_or_else(default_home);
    let mirrors = mirrors(&cli.mirrors)?;
    match cli.cmd {
        Command::Plan { dir, env, json } => {
            let env = parse_env(&env)?;
            let plan = acro_build::plan_app(&dir, &env)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print!("{}", plan.render());
            }
            Ok(())
        }
        Command::Build { dir, tag, env, compression, level, hermetic, keep_work, concurrency } => {
            let env = parse_env(&env)?;
            let compression = Compression::parse(&compression).with_context(|| format!("unknown compression {compression}"))?;
            let target = tag.as_deref().map(Reference::parse).transpose()?;
            let exec: Arc<dyn Executor> = if hermetic == "off" {
                Arc::new(HostExecutor { isolation: Isolation::None })
            } else {
                let e = HostExecutor::detect();
                if e.isolation == Isolation::None {
                    bail!("cannot create network namespaces for hermetic build steps; rerun with --hermetic=off to build without isolation");
                }
                Arc::new(e)
            };
            let opts = BuildOptions {
                app_dir: std::fs::canonicalize(&dir).with_context(|| format!("{} not found", dir.display()))?,
                home,
                target,
                env,
                layer: LayerOptions { compression, level, threads: 0 },
                mirrors,
                concurrency,
                keep_work,
                platform: acro_oci::image::host_platform(),
            };
            let (_plan, res) = acro_build::build(opts, exec).await?;
            acro_events::emit(acro_events::Event::Stats(acro_events::stats()));
            acro_events::emit(acro_events::Event::BuildFinished {
                ms: acro_events::elapsed_ms(),
                image: res.reference.clone(),
                digest: Some(res.manifest_digest.clone()),
            });
            if acro_events::mode() != acro_events::Mode::Json {
                match &res.reference {
                    Some(r) => println!("{r}@{}", res.manifest_digest),
                    None => println!("{}", res.manifest_digest),
                }
            }
            Ok(())
        }
        Command::Inspect { reference } => {
            let r = Reference::parse(&reference)?;
            let client = reqwest::Client::builder().build()?;
            let reg = acro_build::run::make_registry(client, &mirrors);
            let img = reg.resolve(&r, &acro_oci::image::host_platform()).await?;
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
