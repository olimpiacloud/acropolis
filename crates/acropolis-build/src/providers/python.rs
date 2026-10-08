use crate::detect::{Env, VersionSpec, tool_version};
use crate::plan::{Action, LayerFrom, Plan, PlanBuilder};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

pub const DEFAULT_PYTHON: &str = "3.13";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Manager {
    Uv,
    Poetry,
    Pdm,
    Pipenv,
    Pip,
    Pyproject,
    None,
}

pub fn is_python(dir: &Path) -> bool {
    ["requirements.txt", "pyproject.toml", "Pipfile", "main.py", "app.py", "bot.py", "manage.py", "setup.py", "uv.lock", "poetry.lock"]
        .iter()
        .any(|f| dir.join(f).exists())
}

fn read(dir: &Path, f: &str) -> String {
    std::fs::read_to_string(dir.join(f)).unwrap_or_default()
}

pub fn manager(dir: &Path) -> Manager {
    if dir.join("uv.lock").exists() {
        Manager::Uv
    } else if dir.join("poetry.lock").exists() {
        Manager::Poetry
    } else if dir.join("pdm.lock").exists() {
        Manager::Pdm
    } else if dir.join("Pipfile").exists() {
        Manager::Pipenv
    } else if dir.join("requirements.txt").exists() {
        Manager::Pip
    } else if dir.join("pyproject.toml").exists() {
        Manager::Pyproject
    } else {
        Manager::None
    }
}

pub fn version(dir: &Path, env: &Env) -> VersionSpec {
    if let Some((v, k)) = env.config("PYTHON_VERSION") {
        return VersionSpec { spec: v, source: k };
    }
    if let Some(v) = tool_version(dir, "python") {
        return v;
    }
    let pv = read(dir, ".python-version");
    if let Some(l) = pv.lines().map(|l| l.trim()).find(|l| !l.is_empty() && !l.starts_with('#')) {
        return VersionSpec { spec: l.to_string(), source: ".python-version".into() };
    }
    let rt = read(dir, "runtime.txt");
    if let Some(v) = rt.trim().strip_prefix("python-") {
        return VersionSpec { spec: v.to_string(), source: "runtime.txt".into() };
    }
    let pipfile = read(dir, "Pipfile");
    for key in ["python_full_version", "python_version"] {
        for line in pipfile.lines() {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix(key)
                && let Some(v) = rest.split('=').nth(1)
            {
                let v = v.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return VersionSpec { spec: v.to_string(), source: format!("Pipfile {key}") };
                }
            }
        }
    }
    VersionSpec { spec: DEFAULT_PYTHON.into(), source: "default".into() }
}

fn image_for(spec: &str) -> String {
    if matches!(spec.trim(), "latest" | "*" | "3") {
        return "python:3-slim-bookworm".into();
    }
    let s = spec.trim().trim_start_matches("python").trim_start_matches('-').trim();
    let s = s.trim_start_matches(['=', '~', '^', '>', '<', ' ']);
    let s: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let s = if s.is_empty() { DEFAULT_PYTHON.to_string() } else { s };
    format!("python:{s}-slim-bookworm")
}

fn deps_text(dir: &Path) -> String {
    format!("{}\n{}\n{}", read(dir, "requirements.txt"), read(dir, "pyproject.toml"), read(dir, "Pipfile")).to_ascii_lowercase()
}

fn locked(dir: &Path, dep: &str) -> bool {
    let needle = format!("name = \"{dep}\"");
    ["uv.lock", "poetry.lock", "pdm.lock"].iter().any(|f| read(dir, f).to_ascii_lowercase().lines().any(|l| l.trim() == needle))
}

fn uses(dir: &Path, dep: &str) -> bool {
    if locked(dir, dep) {
        return true;
    }
    let text = deps_text(dir);
    text.lines().any(|l| {
        let l = l.trim().trim_start_matches('"').trim_start_matches('\'');
        l.starts_with(dep) && l[dep.len()..].chars().next().map(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_').unwrap_or(true)
    })
}

fn django_settings_text(dir: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() && !name.starts_with('.') && name != "node_modules" && name != "__pycache__" {
                stack.push(p);
            } else if name.ends_with(".py") && let Ok(t) = std::fs::read_to_string(&p) && t.contains("django.db.backends.") {
                out.push_str(&t);
            }
        }
    }
    out
}

fn main_file(dir: &Path) -> Option<&'static str> {
    ["main.py", "app.py", "start.py", "bot.py", "hello.py", "server.py"].into_iter().find(|f| dir.join(f).exists())
}

fn django_app(dir: &Path) -> Option<String> {
    if !dir.join("manage.py").exists() {
        return None;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() && !name.starts_with('.') && name != "node_modules" && name != ".venv" {
                stack.push(p);
            } else if name == "settings.py" {
                let text = std::fs::read_to_string(&p).unwrap_or_default();
                for line in text.lines() {
                    if let Some(rest) = line.trim().strip_prefix("WSGI_APPLICATION") {
                        let v = rest.trim_start_matches([' ', '=']).trim().trim_matches('"').trim_matches('\'');
                        if let Some(module) = v.strip_suffix(".application") {
                            return Some(module.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn start_command(dir: &Path, env: &Env) -> Option<String> {
    if let Some((c, _)) = env.config("START_CMD") {
        return Some(c);
    }
    let procfile = read(dir, "Procfile");
    for line in procfile.lines() {
        if let Some(c) = line.strip_prefix("web:") {
            return Some(c.trim().to_string());
        }
    }
    let mut start = None;
    if let Some(app) = django_app(dir) {
        start = Some(format!("python manage.py migrate && gunicorn --bind 0.0.0.0:${{PORT:-8000}} {app}:application"));
    }
    let main = main_file(dir);
    if main.is_some() && uses(dir, "python-fasthtml") && uses(dir, "uvicorn") {
        start = Some("uvicorn main:app --host 0.0.0.0 --port ${PORT:-8000}".into());
    }
    if main.is_some() && uses(dir, "fastapi") && uses(dir, "uvicorn") {
        start = Some("uvicorn main:app --host 0.0.0.0 --port ${PORT:-8000}".into());
    }
    if main.is_some() && uses(dir, "flask") && uses(dir, "gunicorn") {
        start = Some("gunicorn --bind 0.0.0.0:${PORT:-8000} main:app".into());
    }
    if start.is_none()
        && let Some(m) = main
    {
        start = Some(format!("python {m}"));
    }
    start
}

fn mise_env_truthy(dir: &Path, key: &str) -> bool {
    for file in ["mise.toml", ".mise.toml"] {
        let mut in_env = false;
        for line in read(dir, file).lines() {
            let l = line.trim();
            if l.starts_with('[') {
                in_env = l == "[env]";
                continue;
            }
            if in_env
                && let Some((k, v)) = l.split_once('=')
                && k.trim() == key
            {
                let v = v.trim().trim_matches(['"', '\'']);
                return matches!(v, "1" | "true" | "yes");
            }
        }
    }
    false
}

fn python_minor(spec: &str) -> (u32, u32) {
    let digits: String = spec.trim().trim_start_matches(['=', '~', '^', '>', '<', ' ', 'v']).chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let mut it = digits.split('.').filter_map(|p| p.parse::<u32>().ok());
    match (it.next(), it.next()) {
        (Some(3), Some(m)) => (3, m),
        _ => {
            let d: Vec<u32> = DEFAULT_PYTHON.split('.').filter_map(|p| p.parse().ok()).collect();
            if matches!(spec.trim(), "latest" | "*" | "3") { (3, LATEST_PYTHON_MINOR) } else { (d[0], d[1]) }
        }
    }
}

const LATEST_PYTHON_MINOR: u32 = 14;

fn wheel_fits(wheel: &str, (major, minor): (u32, u32), ft: bool) -> bool {
    let parts: Vec<&str> = wheel.trim_end_matches(".whl").split('-').collect();
    if parts.len() < 5 {
        return false;
    }
    let (py, abi, plat) = (parts[parts.len() - 3], parts[parts.len() - 2], parts[parts.len() - 1]);
    let plat_ok = plat == "any" || plat.split('.').any(|p| (p.starts_with("manylinux") || p.starts_with("linux")) && p.ends_with("x86_64"));
    if !plat_ok {
        return false;
    }
    if abi == "none" {
        return py.split('.').any(|t| t == "py3" || t == format!("py{major}{minor}") || t == format!("cp{major}{minor}"));
    }
    let want = format!("cp{major}{minor}{}", if ft { "t" } else { "" });
    if abi == want {
        return true;
    }
    if abi == "abi3" && !ft {
        return py.split('.').any(|t| t.strip_prefix(&format!("cp{major}")).and_then(|m| m.parse::<u32>().ok()).map(|m| m <= minor).unwrap_or(false));
    }
    false
}

fn needs_compiler(dir: &Path, version: (u32, u32), ft: bool) -> bool {
    let lock = read(dir, "uv.lock");
    lock.split("[[package]]").skip(1).any(|pkg| {
        if !pkg.contains("\nsdist = ") || pkg.contains("source = { virtual") || pkg.contains("source = { editable") {
            return false;
        }
        let wheels: Vec<&str> = pkg.split("url = \"").skip(1).filter_map(|u| u.split('"').next()).filter(|u| u.ends_with(".whl")).map(|u| u.rsplit('/').next().unwrap_or(u)).collect();
        !wheels.iter().any(|w| wheel_fits(w, version, ft))
    })
}

fn uv_lock_installs_nothing(dir: &Path) -> bool {
    let lock = read(dir, "uv.lock");
    let pkgs: Vec<&str> = lock.split("[[package]]").skip(1).collect();
    pkgs.iter().all(|p| p.contains("source = { virtual = \".\" }") && !p.contains("dependencies"))
}

fn freethreaded(dir: &Path, env: &Env, spec: &str) -> bool {
    (spec.ends_with('t') && spec[..spec.len() - 1].ends_with(|c: char| c.is_ascii_digit()))
        || mise_env_truthy(dir, "PYTHON_BUILD_FREE_THREADING")
        || env.vars.get("MISE_PYTHON_PRECOMPILED_FLAVOR").map(|f| f.contains("freethreaded")).unwrap_or(false)
}

const UV_PYTHON_DIR: &str = "/opt/uv-python";

const PLAYWRIGHT_DIR: &str = "/app/.cache/ms-playwright";

const PLAYWRIGHT_DEPS: &str = "libasound2 libatk-bridge2.0-0 libatk1.0-0 libatspi2.0-0 libcairo2 libcups2 libdbus-1-3 libdrm2 libgbm1 libglib2.0-0 libnspr4 libnss3 libpango-1.0-0 libx11-6 libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2";

pub fn plan(dir: &Path, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "python");
    let v = version(dir, env);
    let ft = freethreaded(dir, env, &v.spec);
    let m = manager(dir);
    let image = image_for(&v.spec);
    b.fact("python", format!("{} ({})", v.spec, v.source));
    b.fact("manager", format!("{m:?}").to_ascii_lowercase());
    b.step("base", format!("resolve {image}"), Action::ResolveBase { image: image.clone() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("source", "copy source", Action::CopySource { exclude: vec![".venv".into(), "__pycache__".into(), "**/__pycache__".into()] }, &[]);
    let uv_spec = env.config("UV_VERSION").map(|(v, _)| v).or_else(|| tool_version(dir, "uv").map(|v| v.spec)).unwrap_or_default();
    let uv_spec = if uv_spec == "latest" { String::new() } else { uv_spec };
    b.step("uv", "uv", Action::Toolchain { tool: "uv".into(), spec: uv_spec, parts: vec![] }, &[]);
    let install: Vec<String> = match m {
        Manager::Uv if uv_lock_installs_nothing(dir) => vec![],
        Manager::Uv => vec!["uv sync --locked --no-dev --no-editable".into()],
        Manager::Pip => vec!["uv venv /app/.venv".into(), "uv pip install --python /app/.venv/bin/python -r requirements.txt".into()],
        Manager::Poetry => vec![
            "uv venv /app/.venv".into(),
            "uv tool run --from poetry poetry install --no-interaction --no-ansi --only main --no-root".into(),
        ],
        Manager::Pdm => vec![
            "uv venv /app/.venv".into(),
            "uv tool run --from pdm pdm install --check --prod --no-editable --no-self".into(),
        ],
        Manager::Pipenv => {
            let cmd = if dir.join("Pipfile.lock").exists() { "pipenv install --deploy --ignore-pipfile" } else { "pipenv install --skip-lock" };
            vec!["uv venv /app/.venv".into(), format!("PIPENV_VENV_IN_PROJECT=1 PIPENV_IGNORE_VIRTUALENVS=0 uv tool run --from pipenv {cmd}")]
        }
        Manager::Pyproject => vec!["uv venv /app/.venv".into(), "uv pip install --python /app/.venv/bin/python -r pyproject.toml".into()],
        Manager::None => vec![],
    };
    let mut env_map = BTreeMap::new();
    for (k, v) in [
        ("VIRTUAL_ENV", "/app/.venv"),
        ("UV_PROJECT_ENVIRONMENT", "/app/.venv"),
        ("UV_PYTHON_DOWNLOADS", "never"),
        ("UV_LINK_MODE", "copy"),
        ("UV_COMPILE_BYTECODE", "1"),
        ("UV_CACHE_DIR", "/tmp/uv-cache"),
        ("UV_TOOL_DIR", "/tmp/uv-tools"),
        ("PIP_DISABLE_PIP_VERSION_CHECK", "1"),
        ("PYTHONDONTWRITEBYTECODE", "1"),
    ] {
        env_map.insert(k.to_string(), v.to_string());
    }
    env_map.insert("UV_PYTHON".into(), "/usr/local/bin/python3".into());
    for (k, v) in &env.vars {
        if !k.starts_with("ACROPOLIS_") && !k.starts_with("RAILPACK_") {
            env_map.insert(k.clone(), crate::env_ref(k, v));
        }
    }
    let text = deps_text(dir);
    let mut build_pkgs: Vec<&str> = Vec::new();
    let mut runtime_pkgs: Vec<&str> = Vec::new();
    let binary_psycopg = uses(dir, "psycopg2-binary") || text.contains("psycopg[binary");
    let django_db = |backend: &str| django_settings_text(dir).contains(&format!("django.db.backends.{backend}"));
    if !binary_psycopg && (uses(dir, "psycopg2") || uses(dir, "psycopg") || django_db("postgresql")) {
        build_pkgs.push("libpq-dev");
        runtime_pkgs.push("libpq5");
    }
    if uses(dir, "mysqlclient") || django_db("mysql") {
        build_pkgs.push("default-libmysqlclient-dev");
        runtime_pkgs.push("default-mysql-client");
    }
    for (dep, build, runtime) in [
        ("pycairo", &["libcairo2-dev"][..], &["libcairo2"][..]),
        ("pdf2image", &[], &["poppler-utils"]),
        ("pydub", &[], &["ffmpeg"]),
    ] {
        if uses(dir, dep) {
            build_pkgs.extend(build);
            runtime_pkgs.extend(runtime);
        }
    }
    if needs_compiler(dir, python_minor(&v.spec), ft) {
        build_pkgs.push("g++");
    }
    if text.contains("git+") || read(dir, "uv.lock").contains("git = ") {
        build_pkgs.push("git");
    }
    let extra_build = crate::providers::node::build_apt_packages(env).unwrap_or_default();
    build_pkgs.extend(extra_build.split(' ').filter(|p| !p.is_empty()));
    if !extra_build.is_empty() {
        b.fact("build-apt-packages", extra_build.clone());
    }
    build_pkgs.sort();
    build_pkgs.dedup();
    runtime_pkgs.sort();
    runtime_pkgs.dedup();
    let build_image = if build_pkgs.is_empty() { image.clone() } else { image.replace("-slim-bookworm", "-bookworm") };
    let mut commands = Vec::new();
    if ft {
        let digits: String = v.spec.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let minor = digits.split('.').take(2).collect::<Vec<_>>().join(".");
        let request = format!("{}t", if minor.contains('.') { minor.as_str() } else { "3.14" });
        b.fact("python-build", format!("free-threaded {request} (uv managed)"));
        env_map.insert("UV_PYTHON_INSTALL_DIR".into(), UV_PYTHON_DIR.into());
        env_map.insert("UV_PYTHON_DOWNLOADS".into(), "automatic".into());
        env_map.insert("UV_PYTHON".into(), request.clone());
        commands.push(format!("uv python install {request}"));
        if !install.iter().any(|c| c.starts_with("uv venv") || c.starts_with("uv sync")) {
            commands.push("uv venv /app/.venv".into());
        }
    }
    if !build_pkgs.is_empty() {
        let pkgs = build_pkgs.join(" ");
        commands.push(format!(
            "dpkg -s {pkgs} >/dev/null 2>&1 || (apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends {pkgs} >/dev/null)"
        ));
    }
    commands.extend(install);
    let playwright = env.flag("PYTHON_PLAYWRIGHT_INSTALL");
    if playwright {
        env_map.insert("PLAYWRIGHT_BROWSERS_PATH".into(), PLAYWRIGHT_DIR.into());
        commands.push("/app/.venv/bin/playwright install --only-shell".into());
        runtime_pkgs.extend(PLAYWRIGHT_DEPS.split(' '));
        runtime_pkgs.sort();
        runtime_pkgs.dedup();
    }
    if !runtime_pkgs.is_empty() {
        b.fact("runtime-packages", runtime_pkgs.join(" "));
    }
    if let Some((c, _)) = env.config("BUILD_CMD") {
        commands.push(c);
    }
    let compiled = build_image != image && !commands.is_empty();
    if compiled {
        commands.push(crate::extend::scan_native_libs("/app/.venv /opt/uv-python", crate::extend::NATIVE_DEBS));
    }
    if commands.is_empty() {
        commands.push("true".into());
    }
    b.step(
        "install",
        "install python dependencies",
        Action::ImageRun { image: build_image.clone(), commands, env: env_map, network: true, mount_app: true, after: None, tools: vec!["uv".into()], lowers: vec![] },
        &["source", "uv"],
    );
    b.step("layer-app", "layer app + .venv", Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec![crate::extend::NATIVE_DEBS_FILE.into()] } }, &["install"]);
    let start = start_command(dir, env);
    b.step(
        "layer-uv",
        "layer uv CLI",
        Action::Layer {
            dest: "usr/local/bin".into(),
            from: LayerFrom::Tool { tool: "uv".into(), files: vec![("bin/uv".into(), "uv".into()), ("bin/uvx".into(), "uvx".into())] },
        },
        &["uv"],
    );
    let mut push_deps = vec!["base", "copy-base", "layer-uv", "layer-app"];
    b.plan.image.layers = vec!["layer-uv".into(), "layer-app".into()];
    if compiled {
        b.step(
            "native-libs",
            "install shared libraries needed by compiled packages",
            Action::ImageRun {
                image: image.clone(),
                commands: vec![crate::extend::install_missing_debs(crate::extend::NATIVE_DEBS, &runtime_pkgs)],
                env: BTreeMap::new(),
                network: true,
                mount_app: true,
                after: None,
                tools: vec![],
                lowers: vec![],
            },
            &["install"],
        );
        b.step(
            "layer-native-libs",
            "layer native package libraries",
            Action::Layer {
                dest: "".into(),
                from: LayerFrom::Upper { step: "native-libs".into(), include: vec![], exclude: vec!["var/cache".into(), "var/log".into(), "root".into()] },
            },
            &["native-libs"],
        );
        push_deps.push("layer-native-libs");
        b.plan.image.layers.insert(0, "layer-native-libs".into());
    }
    if ft {
        b.step(
            "layer-python",
            "layer free-threaded python",
            Action::Layer {
                dest: String::new(),
                from: LayerFrom::Upper { step: "install".into(), include: vec![UV_PYTHON_DIR.trim_start_matches('/').into()], exclude: vec![] },
            },
            &["install"],
        );
        push_deps.push("layer-python");
        b.plan.image.layers.insert(0, "layer-python".into());
    }
    b.step("push", "push image", Action::Push, &push_deps);
    b.plan.warnings.push("python dependencies are installed with network access inside the base image".into());
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.env = vec![
        ("PATH".into(), "/app/.venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("VIRTUAL_ENV".into(), "/app/.venv".into()),
        ("PYTHONUNBUFFERED".into(), "1".into()),
    ];
    if playwright {
        b.plan.image.env.push(("PLAYWRIGHT_BROWSERS_PATH".into(), PLAYWRIGHT_DIR.into()));
    }
    if let Some(start) = start {
        b.plan.image.cmd = Some(super::shell_start(&start));
    } else {
        anyhow::bail!("no start command found: add a main.py, a Procfile web entry or set ACROPOLIS_START_CMD");
    }
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freethreaded_spec() {
        let env = Env::default();
        let dir = Path::new("/nonexistent");
        assert!(!freethreaded(dir, &env, "latest"));
        assert!(freethreaded(dir, &env, "3.14t"));
        assert!(!freethreaded(dir, &env, "3.14"));
    }

    #[test]
    fn wheels() {
        let v = (3, 14);
        assert!(wheel_fits("greenlet-3.2.4-cp314-cp314-manylinux_2_24_x86_64.manylinux_2_28_x86_64.whl", v, false));
        assert!(!wheel_fits("greenlet-3.2.4-cp314-cp314-manylinux_2_24_x86_64.whl", v, true));
        assert!(wheel_fits("greenlet-3.2.4-cp314-cp314t-manylinux_2_24_x86_64.whl", v, true));
        assert!(!wheel_fits("greenlet-3.2.4-cp313-cp313-manylinux_2_24_x86_64.whl", v, false));
        assert!(!wheel_fits("greenlet-3.2.4-cp314-cp314-musllinux_1_2_x86_64.whl", v, false));
        assert!(wheel_fits("six-1.16.0-py2.py3-none-any.whl", v, false));
        assert!(wheel_fits("cryptography-44.0.0-cp39-abi3-manylinux_2_28_x86_64.whl", v, false));
        assert!(!wheel_fits("cryptography-44.0.0-cp39-abi3-manylinux_2_28_x86_64.whl", v, true));
    }

    #[test]
    fn minor_versions() {
        assert_eq!(python_minor("3.12.1"), (3, 12));
        assert_eq!(python_minor(">=3.11"), (3, 11));
        assert_eq!(python_minor("latest"), (3, LATEST_PYTHON_MINOR));
    }
}
