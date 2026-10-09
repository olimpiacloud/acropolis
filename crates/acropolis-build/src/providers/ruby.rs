use crate::detect::{Env, VersionSpec, tool_version};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_RUBY: &str = "3.4";

pub const APT_ARCHIVE_FIX: &str = ". /etc/os-release; case \"$VERSION_CODENAME\" in jessie|stretch|buster|bullseye) sed -i -e 's|deb.debian.org|archive.debian.org|g' -e 's|security.debian.org|archive.debian.org|g' -e '/-updates/d' /etc/apt/sources.list; echo 'Acquire::Check-Valid-Until \"false\";' > /etc/apt/apt.conf.d/99acropolis-archive ;; esac";

pub fn is_ruby(dir: &Path) -> bool {
    dir.join("Gemfile").exists()
}

fn read(dir: &Path, f: &str) -> String {
    crate::detect::read_app_file(dir, f).unwrap_or_default()
}

pub fn version(dir: &Path, env: &Env) -> VersionSpec {
    if let Some((v, k)) = env.config("RUBY_VERSION") {
        return VersionSpec { spec: v, source: k };
    }
    if let Some(v) = tool_version(dir, "ruby") {
        return v;
    }
    if let Some(l) = read(dir, ".ruby-version")
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
    {
        return VersionSpec {
            spec: l.trim_start_matches("ruby-").to_string(),
            source: ".ruby-version".into(),
        };
    }
    for line in read(dir, "Gemfile").lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("ruby ") {
            let v = rest.trim().trim_matches('"').trim_matches('\'');
            let v = v
                .trim_start_matches("~>")
                .trim_start_matches(">=")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if !v.is_empty() && v.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                return VersionSpec {
                    spec: v.to_string(),
                    source: "Gemfile".into(),
                };
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
                return VersionSpec {
                    spec: v,
                    source: "Gemfile.lock".into(),
                };
            }
        }
    }
    VersionSpec {
        spec: DEFAULT_RUBY.into(),
        source: "default".into(),
    }
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
    let locked = lock.lines().any(|l| {
        let t = l.trim_start();
        let indent = l.len() - t.len();
        indent == 4 && (t == gem || t.starts_with(&format!("{gem} (")))
    });
    locked
        || read(dir, "Gemfile").lines().any(|l| {
            let t = l.trim_start();
            t.starts_with(&format!("gem '{gem}'")) || t.starts_with(&format!("gem \"{gem}\""))
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
    if matches!(spec.trim(), "latest" | "*") {
        return "ruby:slim".into();
    }
    let v: String = spec
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let v = if v.is_empty() { DEFAULT_RUBY.to_string() } else { v };
    format!("ruby:{v}-slim")
}

const RAILS_SECRET_FALLBACK: &str = r#"[ -n "$SECRET_KEY_BASE$RAILS_MASTER_KEY" ] || export SECRET_KEY_BASE="$(ruby -rsecurerandom -e 'print SecureRandom.hex(64)')""#;

pub fn plan(dir: &Path, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "ruby");
    let v = version(dir, env);
    let image = image_for(&v.spec);
    b.fact("ruby", format!("{} ({})", v.spec, v.source));
    let Some(start) = start_command(dir, env) else {
        bail!("no start command found: add a Procfile web entry, a config.ru or set ACROPOLIS_START_CMD");
    };
    let rails = is_rails(dir);
    if has_gem(dir, "execjs") && !dir.join("package.json").exists() {
        b.fact("extra-node", "lts");
    }
    if rails {
        b.fact("framework", "rails");
    }
    b.step(
        "base",
        format!("resolve {image}"),
        Action::ResolveBase { image: image.clone() },
        &[],
    );
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step(
        "source",
        "copy source",
        Action::CopySource {
            exclude: vec!["vendor/bundle".into(), "tmp".into(), "log/*.log".into()],
        },
        &[],
    );
    let pg = has_gem(dir, "pg");
    let mysql = has_gem(dir, "mysql2");
    let build_image = if image == "ruby:slim" {
        "ruby:latest".to_string()
    } else {
        image.trim_end_matches("-slim").to_string()
    };
    let mut commands: Vec<String> = Vec::new();
    if let Some(bv) = bundler_version(dir) {
        commands.push(format!(
            "(gem list -i bundler -v {bv} >/dev/null || gem install -N bundler -v {bv})"
        ));
    }
    commands.push("bundle install --jobs 4".into());
    let has_node = dir.join("package.json").exists();
    if rails && dir.join("app/assets").exists() && !has_node {
        commands.push("SECRET_KEY_BASE_DUMMY=1 SECRET_KEY_BASE=\"${SECRET_KEY_BASE:-assets-precompile-placeholder}\" bundle exec rake assets:precompile".into());
    }
    if let Some((c, _)) = env.config("BUILD_CMD") {
        commands.push(c);
    }
    commands.push(crate::extend::scan_native_libs(
        "/usr/local/bundle",
        crate::extend::NATIVE_DEBS,
    ));
    let fixed_env = [
        ("BUNDLE_GEMFILE", "/app/Gemfile"),
        ("BUNDLE_WITHOUT", "development:test"),
        ("RACK_ENV", "production"),
        ("RAILS_ENV", "production"),
    ];
    let mut run_env: BTreeMap<String, String> = fixed_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    run_env.extend(crate::user_env(env));
    b.step(
        "install",
        format!("bundle install (in {build_image})"),
        Action::ImageRun {
            image: build_image.clone(),
            commands,
            env: run_env.clone(),
            network: true,
            mount_app: true,
            after: None,
            tools: vec![],
            lowers: vec![],
        },
        &["source"],
    );
    b.step(
        "layer-app",
        "layer app",
        Action::Layer {
            dest: "app".into(),
            from: LayerFrom::WorkDir {
                path: ".".into(),
                exclude: vec![crate::extend::NATIVE_DEBS_FILE.into()],
            },
        },
        &["install"],
    );
    b.step(
        "layer-gems",
        "layer gems (/usr/local/bundle)",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Upper {
                step: "install".into(),
                include: vec!["usr/local/bundle".into()],
                exclude: vec!["usr/local/bundle/cache".into()],
            },
        },
        &["install"],
    );
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
                "{APT_ARCHIVE_FIX}; apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {} >/dev/null && rm -rf /var/lib/apt/lists/*",
                runtime_pkgs.join(" ")
            )],
            env: BTreeMap::new(),
            network: true,
            mount_app: false,
            after: None,
            tools: vec![],
            lowers: vec![],
        },
        &[],
    );
    b.step(
        "layer-libs",
        "layer runtime libraries",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Upper {
                step: "runtime-libs".into(),
                include: vec![],
                exclude: vec!["var/cache".into(), "var/log".into(), "root".into()],
            },
        },
        &["runtime-libs"],
    );
    b.step(
        "native-libs",
        "install shared libraries needed by native gems",
        Action::ImageRun {
            image: image.clone(),
            commands: vec![crate::extend::install_missing_debs(crate::extend::NATIVE_DEBS, &[])],
            env: BTreeMap::new(),
            network: true,
            mount_app: true,
            after: None,
            tools: vec![],
            lowers: vec!["runtime-libs".into()],
        },
        &["runtime-libs", "install"],
    );
    b.step(
        "layer-native-libs",
        "layer native gem libraries",
        Action::Layer {
            dest: "".into(),
            from: LayerFrom::Upper {
                step: "native-libs".into(),
                include: vec![],
                exclude: vec!["var/cache".into(), "var/log".into(), "root".into()],
            },
        },
        &["native-libs"],
    );
    b.step(
        "push",
        "push image",
        Action::Push,
        &[
            "base",
            "copy-base",
            "layer-libs",
            "layer-native-libs",
            "layer-gems",
            "layer-app",
        ],
    );
    b.plan
        .warnings
        .push("gems are installed with network access inside the base image".into());
    b.plan.image.layers = vec![
        "layer-libs".into(),
        "layer-native-libs".into(),
        "layer-gems".into(),
        "layer-app".into(),
    ];
    b.plan.image.workdir = Some("/app".into());
    // Only acropolis' own values: user variables are `{env:NAME}` placeholders that exist only in build steps.
    let mut img_env: Vec<(String, String)> = fixed_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    img_env.push(("LD_PRELOAD".into(), "libjemalloc.so.2".into()));
    img_env.push(("RUBY_YJIT_ENABLE".into(), "1".into()));
    img_env.push(("RAILS_LOG_TO_STDOUT".into(), "enabled".into()));
    img_env.push(("RAILS_SERVE_STATIC_FILES".into(), "true".into()));
    b.plan.image.env = img_env;
    let start = if rails
        && !dir.join("config/master.key").exists()
        && !dir.join("config/credentials/production.key").exists()
    {
        format!("{RAILS_SECRET_FALLBACK}; {start}")
    } else {
        start
    };
    b.plan.image.cmd = Some(super::shell_start(&start));
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_variables_do_not_reach_the_image_env_as_placeholders() {
        let dir = crate::detect::scratch_dir("ruby-env");
        std::fs::write(dir.join("Gemfile"), "source 'https://rubygems.org'\n").unwrap();
        std::fs::write(dir.join("config.ru"), "run ->(_) { [200, {}, ['ok']] }\n").unwrap();
        let mut env = Env::default();
        env.vars.insert("RAILS_ENV".into(), "staging".into());
        env.vars.insert("BUNDLE_GITHUB__COM".into(), "token".into());
        let p = plan(&dir, &env, "app").unwrap();
        assert!(
            p.image.env.iter().all(|(_, v)| !v.contains("{env:")),
            "{:?}",
            p.image.env
        );
        assert!(p.image.env.iter().any(|(k, v)| k == "RAILS_ENV" && v == "production"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
