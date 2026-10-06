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
    for line in pipfile.lines() {
        let l = line.trim();
        for key in ["python_full_version", "python_version"] {
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
    let s = spec.trim().trim_start_matches("python").trim_start_matches('-').trim();
    let s = s.trim_start_matches(['=', '~', '^', '>', '<', ' ']);
    let s: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let s = if s.is_empty() { DEFAULT_PYTHON.to_string() } else { s };
    format!("python:{s}-slim-bookworm")
}

fn deps_text(dir: &Path) -> String {
    format!("{}\n{}\n{}", read(dir, "requirements.txt"), read(dir, "pyproject.toml"), read(dir, "Pipfile")).to_ascii_lowercase()
}

fn uses(dir: &Path, dep: &str) -> bool {
    let text = deps_text(dir);
    text.lines().any(|l| {
        let l = l.trim().trim_start_matches('"').trim_start_matches('\'');
        l.starts_with(dep) && l[dep.len()..].chars().next().map(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_').unwrap_or(true)
    })
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

pub fn plan(dir: &Path, env: &Env, name: &str) -> Result<Plan> {
    let mut b = PlanBuilder::new(name, "python");
    let v = version(dir, env);
    let m = manager(dir);
    let image = image_for(&v.spec);
    b.fact("python", format!("{} ({})", v.spec, v.source));
    b.fact("manager", format!("{m:?}").to_ascii_lowercase());
    b.step("base", format!("resolve {image}"), Action::ResolveBase { image: image.clone() }, &[]);
    b.step("copy-base", "copy base layers", Action::CopyBase, &["base"]);
    b.step("source", "copy source", Action::CopySource { exclude: vec![".venv".into(), "__pycache__".into(), "**/__pycache__".into()] }, &[]);
    b.step("uv", "uv", Action::Toolchain { tool: "uv".into(), spec: env.config("UV_VERSION").map(|(v, _)| v).unwrap_or_default(), parts: vec![] }, &[]);
    let install: Vec<String> = match m {
        Manager::Uv => vec!["uv sync --locked --no-dev --no-editable".into()],
        Manager::Pip => vec!["uv venv /app/.venv".into(), "uv pip install -r requirements.txt".into()],
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
        Manager::Pyproject => vec!["uv venv /app/.venv".into(), "uv pip install -r pyproject.toml".into()],
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
        if !k.starts_with("ACRO_") && !k.starts_with("RAILPACK_") {
            env_map.insert(k.clone(), v.clone());
        }
    }
    let mut commands = install;
    if let Some((c, _)) = env.config("BUILD_CMD") {
        commands.push(c);
    }
    if commands.is_empty() {
        commands.push("true".into());
    }
    b.step(
        "install",
        "install python dependencies",
        Action::ImageRun { image: image.clone(), commands, env: env_map, network: true, mount_app: true, after: None, tools: vec!["uv".into()] },
        &["source", "uv"],
    );
    b.step("layer-app", "layer app + .venv", Action::Layer { dest: "app".into(), from: LayerFrom::WorkDir { path: ".".into(), exclude: vec![] } }, &["install"]);
    b.step("push", "push image", Action::Push, &["base", "copy-base", "layer-app"]);
    b.plan.warnings.push("python dependencies are installed with network access inside the base image".into());
    b.plan.image.layers = vec!["layer-app".into()];
    b.plan.image.workdir = Some("/app".into());
    b.plan.image.env = vec![
        ("PATH".into(), "/app/.venv/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("VIRTUAL_ENV".into(), "/app/.venv".into()),
        ("PYTHONUNBUFFERED".into(), "1".into()),
    ];
    if let Some(start) = start_command(dir, env) {
        b.plan.image.cmd = Some(vec!["/bin/sh".into(), "-c".into(), start]);
    } else {
        anyhow::bail!("no start command found: add a main.py, a Procfile web entry or set ACRO_START_CMD");
    }
    b.plan.image.entrypoint = Some(vec![]);
    Ok(b.finish())
}
