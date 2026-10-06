use crate::detect::{Env, VersionSpec, tool_version};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_RUBY: &str = "3.4";

pub fn is_ruby(dir: &Path) -> bool {
    dir.join("Gemfile").exists()
}

fn read(dir: &Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap_or_default()
}

pub fn version(dir: &Path, env: &Env) -> VersionSpec {
    if let Some((v, k)) = env.config("RUBY_VERSION") {
        return VersionSpec { spec: v, source: k };
    }
    if let Some(v) = tool_version(dir, "ruby") {
        return v;
    }
    if let Some(l) = read(dir, ".ruby-version").lines().map(|l| l.trim()).find(|l| !l.is_empty()) {
        return VersionSpec { spec: l.trim_start_matches("ruby-").to_string(), source: ".ruby-version".into() };
    }
    for line in read(dir, "Gemfile").lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("ruby ") {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            let v = v.trim_start_matches("~>").trim_start_matches(">=").trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() && v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return VersionSpec { spec: v.to_string(), source: "Gemfile".into() };
            }
        }
    }
    let lock = read(dir, "Gemfile.lock");
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "RUBY VERSION"
            && let Some(next) = lines.next()
            && let Some(v) = next.trim().strip_prefix("ruby ")
        {
            let v: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if !v.is_empty() {
                return VersionSpec { spec: v, source: "Gemfile.lock".into() };
            }
        }
    }
    VersionSpec { spec: DEFAULT_RUBY.into(), source: "default".into() }
}

fn bundler_version(dir: &Path) -> Option<String> {
    let lock = read(dir, "Gemfile.lock");
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == "BUNDLED WITH" {
            return lines.next().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        }
    }
    None
}

fn has_gem(dir: &Path, gem: &str) -> bool {
    let lock = read(dir, "Gemfile.lock");
    lock.lines().any(|l| {
        let t = l.trim_start();
        let indent = l.len() - t.len();
        indent == 4 && (t == gem || t.starts_with(&format!("{gem} (")))
    })
}

fn is_rails(dir: &Path) -> bool {
    dir.join("config/application.rb").exists() && has_gem(dir, "rails")
}

pub fn start_command(dir: &Path, env: &Env) -> Option<String> {
    if let Some((c, _)) = env.config("START_CMD") {
        return Some(c);
    }
    for line in read(dir, "Procfile").lines() {
        if let Some(c) = line.strip_prefix("web:") {
            return Some(c.trim().to_string());
        }
    }
    if is_rails(dir) {
        if dir.join("bin/rails").exists() {
            return Some("bundle exec bin/rails server -b 0.0.0.0 -p ${PORT:-3000} -e $RAILS_ENV".into());
        }
        return Some("bundle exec rails server -b 0.0.0.0 -p ${PORT:-3000}".into());
    }
    if dir.join("config.ru").exists() {
        return Some("bundle exec rackup config.ru -o 0.0.0.0 -p ${PORT:-3000}".into());
    }
    if dir.join("Rakefile").exists() {
        return Some("bundle exec rake".into());
    }
    None
}

fn image_for(spec: &str) -> String {
    let v: String = spec.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let v = if v.is_empty() { DEFAULT_RUBY.to_string() } else { v };
    format!("ruby:{v}-slim")
}

pub fn plan(dir: &Path, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "ruby");
    let v = version(dir, env);
    let image = image_for(&v.spec);
    b.fact("ruby", format!("{} ({})", v.spec, v.source));
    let Some(start) = start_command(dir, env) else {
        bail!("no start command found: add a Procfile web entry, a config.ru or set ACRO_START_CMD");
    };
    let rails = is_rails(dir);
    if rails {
        b.fact("framework", "rails");
    }
    b.step("base", format!("resolve {image}"), Action::ResolveBase { image: image.clone() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("source", "copy source", Action::CopySource { exclude: vec!["vendor/bundle".into(), "tmp".into(), "log/*.log".into()] }, &[]);
    let pg = has_gem(dir, "pg");
    let mysql = has_gem(dir, "mysql2");
    let mut build_pkgs = vec!["build-essential", "libyaml-dev", "git", "pkg-config"];
    if pg {
        build_pkgs.push("libpq-dev");
    }
    if mysql {
        build_pkgs.push("default-libmysqlclient-dev");
    }
    let mut commands = vec![format!(
        "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {} >/dev/null",
        build_pkgs.join(" ")
    )];
    if let Some(bv) = bundler_version(dir) {
        commands.push(format!("(gem list -i bundler -v {bv} >/dev/null || gem install -N bundler -v {bv})"));
    }
    commands.push("bundle install --jobs 4".into());
    let has_node = dir.join("package.json").exists();
    if rails && dir.join("app/assets").exists() && !has_node {
        commands.push("SECRET_KEY_BASE_DUMMY=1 bundle exec rake assets:precompile".into());
    }
    if let Some((c, _)) = env.config("BUILD_CMD") {
        commands.push(c);
    }
    let mut run_env = BTreeMap::new();
    for (k, v) in [
        ("BUNDLE_PATH", "/app/vendor/bundle"),
        ("BUNDLE_WITHOUT", "development:test"),
        ("BUNDLE_GEMFILE", "/app/Gemfile"),
        ("RAILS_ENV", "production"),
        ("RACK_ENV", "production"),
    ] {
        run_env.insert(k.to_string(), v.to_string());
    }
    for (k, v) in &env.vars {
        if !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_") {
            run_env.insert(k.clone(), v.clone());
        }
    }
    b.step(
        "install",
        "bundle install",
        Action::ImageRun { image: image.clone(), commands, env: run_env.clone(), network: true, mount_app: true, after: None, tools: vec![] },
        &["source"],
    );
    b.step("layer-app", "layer app + gems", Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec![] } }, &["install"]);
    let mut runtime_pkgs = vec!["libjemalloc2"];
    if pg {
        runtime_pkgs.push("libpq5");
    }
    if mysql {
        runtime_pkgs.push("libmariadb3");
    }
    b.step(
        "runtime-libs",
        format!("apt-get install {}", runtime_pkgs.join(" ")),
        Action::ImageRun {
            image: image.clone(),
            commands: vec![format!(
                "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {} >/dev/null && rm -rf /var/lib/apt/lists/*",
                runtime_pkgs.join(" ")
            )],
            env: BTreeMap::new(),
            network: true,
            mount_app: false,
            after: None,
            tools: vec![],
        },
        &[],
    );
    b.step(
        "layer-libs",
        "layer runtime libraries",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Upper { step: "runtime-libs".into(), include: vec![], exclude: vec!["var/cache".into(), "var/log".into(), "root".into()] },
        },
        &["runtime-libs"],
    );
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-libs", "layer-app"]);
    b.plan.warnings.push("gems are installed with network access inside the base image".into());
    b.plan.image.layers = vec!["layer-libs".into(), "layer-app".into()];
    b.plan.image.workdir = Some("/app".into());
    let mut img_env: Vec<(String, String)> = run_env
        .iter()
        .filter(|(k, _)| k.starts_with("BUNDLE_") || *k == "RAILS_ENV" || *k == "RACK_ENV")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    img_env.push(("LD_PRELOAD".into(), "libjemalloc.so.2".into()));
    img_env.push(("RUBY_YJIT_ENABLE".into(), "1".into()));
    img_env.push(("RAILS_LOG_TO_STDOUT".into(), "enabled".into()));
    img_env.push(("RAILS_SERVE_STATIC_FILES".into(), "true".into()));
    b.plan.image.env = img_env;
    b.plan.image.cmd = Some(vec!["/bin/sh".into(), "-c".into(), start]);
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
