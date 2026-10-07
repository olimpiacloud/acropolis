use crate::detect::{Env, tool_version};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

pub struct ImageBuild {
    pub provider: &'static str,
    pub base: Action,
    pub build_image: String,
    pub commands: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub outputs: Vec<(String, String)>,
    pub cmd: String,
    pub image_env: Vec<(String, String)>,
    pub facts: Vec<(String, String)>,
}

fn read(dir: &Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap_or_default()
}

fn user_env(env: &Env) -> BTreeMap<String, String> {
    env.vars
        .iter()
        .filter(|(k, _)| !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_"))
        .map(|(k, v)| (k.clone(), crate::env_ref(k, v)))
        .collect()
}

pub fn plan(dir: &Path, env: &Env, name: &str, spec: ImageBuild) -> Result<Plan> {
    let _ = dir;
    let mut b = PlanBuilder::new(name, spec.provider);
    for (k, v) in &spec.facts {
        b.fact(k, v.clone());
    }
    b.step("base", "resolve runtime image", spec.base, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("source", "copy source", Action::CopySource { exclude: vec![] }, &[]);
    let mut run_env = spec.env.clone();
    run_env.extend(user_env(env));
    let mut commands = spec.commands.clone();
    if let Some((c, _)) = env.config("BUILD_CMD") {
        commands.push(c);
    }
    let deps: Vec<&str> = if spec.build_image.starts_with("@base") { vec!["source", "base"] } else { vec!["source"] };
    b.step(
        "build",
        format!("build in {}", spec.build_image),
        Action::ImageRun { image: spec.build_image.clone(), commands, env: run_env, network: true, mount_app: true, after: None, tools: vec![], lowers: vec![] },
        &deps,
    );
    let from = if spec.outputs.is_empty() {
        LayerFrom::WorkDir { path: ".".into(), exclude: vec![] }
    } else {
        LayerFrom::Paths { items: spec.outputs.clone() }
    };
    b.step("layer-app", "layer build output", Action::Layer { dest: "app".into(), from }, &["build"]);
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-app"]);
    b.plan.warnings.push(format!("{} dependencies are fetched with network access inside the build image", spec.provider));
    b.plan.image.layers = vec!["layer-app".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.env = spec.image_env.clone();
    let start = env.config("START_CMD").map(|(c, _)| c).unwrap_or(spec.cmd.clone());
    b.plan.image.cmd = Some(vec!["/bin/sh".into(), "-c".into(), start]);
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}

pub fn detect(dir: &Path, env: &Env) -> Option<Result<ImageBuild>> {
    let forced = env.config("PROVIDER").map(|(p, _)| p);
    let is = |p: &str, cond: bool| forced.as_deref() == Some(p) || (forced.is_none() && cond);
    if is("deno", dir.join("deno.json").exists() || dir.join("deno.jsonc").exists() || dir.join(".deno-version").exists()) {
        return Some(deno(dir, env));
    }
    if is("gleam", dir.join("gleam.toml").exists()) {
        return Some(gleam(dir, env));
    }
    if is("dotnet", has_ext(dir, "csproj") || has_ext(dir, "fsproj") || has_ext(dir, "sln")) {
        return Some(dotnet(dir, env));
    }
    if is("java", dir.join("pom.xml").exists() || dir.join("gradlew").exists() || dir.join("build.gradle").exists() || dir.join("build.gradle.kts").exists()) {
        return Some(java(dir, env));
    }
    if is("elixir", dir.join("mix.exs").exists()) {
        return Some(elixir(dir, env));
    }
    if is("cpp", dir.join("CMakeLists.txt").exists() || dir.join("meson.build").exists()) {
        return Some(cpp(dir, env));
    }
    None
}

fn has_ext(dir: &Path, ext: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().ends_with(&format!(".{ext}"))))
        .unwrap_or(false)
}

fn first_with_ext(dir: &Path, ext: &str) -> Option<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(&format!(".{ext}")))
        .collect();
    v.sort();
    v.into_iter().next()
}

fn deno(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let version = env
        .config("DENO_VERSION")
        .map(|(v, _)| v)
        .or_else(|| tool_version(dir, "deno").map(|v| v.spec))
        .or_else(|| read(dir, ".deno-version").lines().next().map(|l| l.trim().trim_start_matches('v').to_string()).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "2".into());
    let main = ["main.ts", "main.js", "mod.ts", "index.ts", "index.js", "src/main.ts"]
        .iter()
        .find(|f| dir.join(f).exists())
        .map(|s| s.to_string());
    let Some(main) = main else { bail!("no Deno entrypoint found (main.ts, mod.ts, index.ts)") };
    let image = format!("denoland/deno:{version}");
    let mut e = BTreeMap::new();
    e.insert("DENO_DIR".to_string(), "/app/.deno".to_string());
    Ok(ImageBuild {
        provider: "deno",
        base: Action::ResolveBase { image: image.clone() },
        build_image: image,
        commands: vec![format!("deno cache {main}")],
        env: e,
        outputs: vec![],
        cmd: format!("deno run --allow-all {main}"),
        image_env: vec![("DENO_DIR".into(), "/app/.deno".into())],
        facts: vec![("deno".into(), version)],
    })
}

fn gleam(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let version = env
        .config("GLEAM_VERSION")
        .map(|(v, _)| v)
        .or_else(|| tool_version(dir, "gleam").map(|v| v.spec));
    let base = match &version {
        Some(v) => Action::ResolveBase { image: format!("ghcr.io/gleam-lang/gleam:v{}-erlang-slim", v.trim_start_matches('v')) },
        None => Action::ResolveBaseLatest { template: "ghcr.io/gleam-lang/gleam:{tag}-erlang-slim".into(), github: "gleam-lang/gleam".into() },
    };
    let include_source = env.flag("GLEAM_INCLUDE_SOURCE");
    let outputs = if include_source { vec![] } else { vec![("build/erlang-shipment".into(), "build/erlang-shipment".into())] };
    Ok(ImageBuild {
        provider: "gleam",
        base,
        build_image: "@base-variant:-erlang-slim=-erlang".into(),
        commands: vec!["gleam export erlang-shipment".into()],
        env: BTreeMap::new(),
        outputs,
        cmd: "./build/erlang-shipment/entrypoint.sh run".into(),
        image_env: vec![],
        facts: vec![("gleam".into(), version.unwrap_or_else(|| "latest".into()))],
    })
}

fn dotnet(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let proj = first_with_ext(dir, "csproj").or_else(|| first_with_ext(dir, "fsproj"));
    let Some(proj) = proj else { bail!("no .csproj or .fsproj found") };
    let text = read(dir, &proj);
    let tfm = text
        .split("<TargetFramework>")
        .nth(1)
        .and_then(|s| s.split("</TargetFramework>").next())
        .unwrap_or("net8.0")
        .trim()
        .to_string();
    let runtime_version = tfm.trim_start_matches("net").to_string();
    let sdk = env
        .config("DOTNET_VERSION")
        .map(|(v, _)| v)
        .or_else(|| {
            read(dir, "global.json")
                .split("\"version\"")
                .nth(1)
                .and_then(|s| s.split('"').nth(1))
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| {
            let major: u32 = runtime_version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(8);
            if major < 8 { "8.0".into() } else { runtime_version.clone() }
        });
    let web = text.contains("Microsoft.NET.Sdk.Web");
    let runtime = if web { "aspnet" } else { "runtime" };
    let assembly = text
        .split("<AssemblyName>")
        .nth(1)
        .and_then(|s| s.split("</AssemblyName>").next())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| proj.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or(proj.clone()));
    let mut e = BTreeMap::new();
    e.insert("DOTNET_CLI_TELEMETRY_OPTOUT".into(), "1".into());
    e.insert("DOTNET_NOLOGO".into(), "1".into());
    e.insert("DOTNET_ROLL_FORWARD".into(), "Major".into());
    Ok(ImageBuild {
        provider: "dotnet",
        base: Action::ResolveBase { image: format!("mcr.microsoft.com/dotnet/{runtime}:{runtime_version}") },
        build_image: format!("mcr.microsoft.com/dotnet/sdk:{sdk}"),
        commands: vec!["dotnet restore".into(), "dotnet publish --no-restore -c Release -o out".into()],
        env: e,
        outputs: vec![("out".into(), "out".into())],
        cmd: format!("dotnet out/{assembly}.dll"),
        image_env: vec![("ASPNETCORE_URLS".into(), "http://0.0.0.0:8080".into()), ("DOTNET_CLI_TELEMETRY_OPTOUT".into(), "1".into())],
        facts: vec![("sdk".into(), sdk), ("framework".into(), tfm)],
    })
}

fn java(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let pom = read(dir, "pom.xml");
    let gradle = dir.join("gradlew").exists();
    let from_pom = ["maven.compiler.release", "java.version", "maven.compiler.source", "maven.compiler.target"]
        .iter()
        .find_map(|k| pom.split(&format!("<{k}>")).nth(1).and_then(|s| s.split('<').next()).map(|s| s.trim().trim_start_matches("1.").to_string()));
    let jdk = env
        .config("JDK_VERSION")
        .map(|(v, _)| v)
        .or_else(|| env.vars.get("ACRO_JAVA_PACKAGE").map(|v| v.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect::<String>().split('.').next().unwrap_or("21").to_string()))
        .or(from_pom)
        .unwrap_or_else(|| "21".into());
    let jdk = if jdk.is_empty() { "21".to_string() } else { jdk };
    let (build_image, commands, cmd) = if gradle {
        (
            format!("eclipse-temurin:{jdk}-jdk"),
            vec!["chmod +x gradlew".into(), "./gradlew clean build -x check -x test -Pproduction".into()],
            "java $JAVA_OPTS -jar $(ls -1 */build/libs/*jar build/libs/*jar 2>/dev/null | grep -v plain | head -1)".to_string(),
        )
    } else {
        let mvn = if dir.join("mvnw").exists() { "chmod +x mvnw && ./mvnw" } else { "mvn" };
        (
            format!("maven:3-eclipse-temurin-{jdk}"),
            vec![format!("{mvn} -B -DskipTests clean install -Pproduction")],
            "java $JAVA_OPTS -jar $(ls -1 target/*.jar | head -1)".to_string(),
        )
    };
    Ok(ImageBuild {
        provider: "java",
        base: Action::ResolveBase { image: format!("eclipse-temurin:{jdk}-jre") },
        build_image,
        commands,
        env: BTreeMap::new(),
        outputs: vec![],
        cmd,
        image_env: vec![],
        facts: vec![("jdk".into(), jdk), ("build".into(), if gradle { "gradle".into() } else { "maven".into() })],
    })
}

fn cpp(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let _ = env;
    let (tools, commands, exe) = if dir.join("CMakeLists.txt").exists() {
        let text = read(dir, "CMakeLists.txt");
        let exe = text.split("add_executable(").nth(1).and_then(|s| s.split([' ', ')']).next()).unwrap_or("app").trim().to_string();
        ("build-essential cmake", vec!["cmake -B build -DCMAKE_BUILD_TYPE=Release".to_string(), "cmake --build build --parallel".to_string()], exe)
    } else {
        let text = read(dir, "meson.build");
        let exe = text
            .split("executable(")
            .nth(1)
            .and_then(|s| s.split(['\'', '"']).nth(1))
            .unwrap_or("app")
            .to_string();
        ("build-essential meson ninja-build", vec!["meson setup build --buildtype=release".to_string(), "meson compile -C build".to_string()], exe)
    };
    let mut cmds = vec![format!(
        "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {tools} >/dev/null"
    )];
    cmds.extend(commands);
    Ok(ImageBuild {
        provider: "cpp",
        base: Action::ResolveBase { image: "debian:bookworm-slim".into() },
        build_image: "debian:bookworm".into(),
        commands: cmds,
        env: BTreeMap::new(),
        outputs: vec![(format!("build/{exe}"), format!("build/{exe}"))],
        cmd: format!("./build/{exe}"),
        image_env: vec![],
        facts: vec![("executable".into(), exe)],
    })
}

fn elixir(dir: &Path, env: &Env) -> Result<ImageBuild> {
    let mix = read(dir, "mix.exs");
    let app = mix
        .split("app:")
        .nth(1)
        .and_then(|r| r.trim_start().strip_prefix(':'))
        .map(|r| r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|s| !s.is_empty());
    let Some(app) = app else { bail!("could not find the application name in mix.exs") };
    let mix_version = mix.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("elixir:")?;
        let req = rest.split('"').nth(1)?;
        let v: String = req.trim_start_matches(['~', '>', '=', '<', ' ']).chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        (!v.is_empty()).then_some(v)
    });
    let version = tool_version(dir, "elixir")
        .map(|v| v.spec)
        .or_else(|| env.config("ELIXIR_VERSION").map(|(v, _)| v))
        .or_else(|| Some(read(dir, ".elixir-version").trim().to_string()).filter(|v| !v.is_empty()))
        .or(mix_version)
        .unwrap_or_else(|| "1.18".into());
    let tag = if version == "latest" { "latest".to_string() } else { acro_semver::fuzzy_version(&version) };
    let image = format!("elixir:{tag}");
    let mut commands = vec![
        "mkdir -p config deps _build".to_string(),
        "mix local.hex --force".to_string(),
        "mix local.rebar --force".to_string(),
        "mix deps.get --only prod".to_string(),
        "mix deps.compile".to_string(),
        "mix compile".to_string(),
    ];
    if mix.contains("\"assets.deploy\"") || mix.contains("assets.deploy:") {
        commands.push("mix assets.deploy".into());
    }
    if mix.contains("\"ecto.deploy\"") || mix.contains("ecto.deploy:") {
        commands.push("mix ecto.deploy".into());
    }
    commands.push("mix release --overwrite".into());
    let mut e = BTreeMap::new();
    for (k, v) in [("MIX_ENV", "prod"), ("MIX_HOME", "/root/.mix"), ("HEX_HOME", "/root/.hex"), ("ELIXIR_ERL_OPTIONS", "+fnu"), ("LANG", "C.UTF-8")] {
        e.insert(k.to_string(), v.to_string());
    }
    let rel = format!("_build/prod/rel/{app}");
    Ok(ImageBuild {
        provider: "elixir",
        base: Action::ResolveBase { image: format!("@slim-of:{image}") },
        build_image: image.clone(),
        commands,
        env: e,
        outputs: vec![(rel.clone(), rel.clone())],
        cmd: format!("/app/{rel}/bin/{app} start"),
        image_env: vec![("MIX_ENV".into(), "prod".into()), ("LANG".into(), "C.UTF-8".into()), ("PORT".into(), "4000".into())],
        facts: vec![("elixir".into(), tag), ("app".into(), app), ("runtime-packages".into(), "libstdc++6 openssl libncurses6 ca-certificates".into())],
    })
}
