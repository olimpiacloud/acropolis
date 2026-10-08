use crate::detect::Env;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepConfig {
    #[serde(default)]
    pub commands: Vec<Value>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub deploy_outputs: Option<Vec<Value>>,
    #[serde(default)]
    pub inputs: Option<Vec<Value>>,
    #[serde(default)]
    pub caches: Option<Vec<String>>,
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
    #[serde(default)]
    pub uses_secrets: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeployConfig {
    #[serde(default)]
    pub start_command: Option<String>,
    #[serde(default)]
    pub apt_packages: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<Value>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub base: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "$schema")]
    pub schema: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub build_apt_packages: Vec<String>,
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
    #[serde(default)]
    pub steps: BTreeMap<String, StepConfig>,
    #[serde(default)]
    pub deploy: Option<DeployConfig>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub caches: Option<Value>,
}

pub fn load(dir: &Path, env: &Env) -> Result<Option<Config>> {
    let explicit = env
        .vars
        .get("ACROPOLIS_CONFIG_FILE")
        .cloned()
        .or_else(|| env.config("CONFIG_FILE").map(|(v, _)| v));
    let path = match &explicit {
        Some(p) => {
            let rel = Path::new(p);
            if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                anyhow::bail!("config file {p:?} must be a relative path inside the app directory");
            }
            let path = dir.join(rel);
            if let (Ok(real), Ok(root)) = (std::fs::canonicalize(&path), std::fs::canonicalize(dir))
                && !real.starts_with(&root)
            {
                anyhow::bail!("config file {p:?} resolves outside the app directory");
            }
            path
        }
        None => {
            let found = ["acropolis.json", "railpack.json"]
                .iter()
                .map(|f| dir.join(f))
                .find(|p| p.exists());
            match found {
                Some(p) => p,
                None => return Ok(None),
            }
        }
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading config {}", path.display()))?;
    let value: Value = json5::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    let cfg: Config = serde_json::from_value(value)
        .with_context(|| format!("{} does not match the config schema", path.display()))?;
    Ok(Some(cfg))
}

const INSTALL_COMMANDS: &[&str] = &[
    "npm ci",
    "npm install",
    "npm i",
    "pnpm install",
    "pnpm i",
    "yarn",
    "yarn install",
    "bun install",
    "pnpm install --frozen-lockfile",
    "yarn install --frozen-lockfile",
];

fn command_strings(step: &StepConfig) -> Vec<String> {
    step.commands
        .iter()
        .filter_map(|c| c.as_str().map(|s| s.to_string()))
        .collect()
}

pub fn apply(cfg: &Config, env: &mut Env) -> Result<()> {
    let mut unsupported = Vec::new();
    let build_apt: Vec<String> = cfg
        .build_apt_packages
        .iter()
        .filter(|p| p.as_str() != "...")
        .cloned()
        .collect();
    if !build_apt.is_empty() {
        env.vars
            .insert("ACROPOLIS_BUILD_APT_PACKAGES".into(), build_apt.join(" "));
    }
    let mut other_packages = Vec::new();
    for (k, v) in &cfg.packages {
        match k.as_str() {
            "java" => {
                env.vars.insert("ACROPOLIS_JAVA_PACKAGE".into(), v.clone());
            }
            "node" | "go" | "python" | "ruby" | "bun" | "deno" | "rust" => {
                env.vars
                    .entry(format!("ACROPOLIS_{}_VERSION", k.to_ascii_uppercase()))
                    .or_insert_with(|| v.clone());
            }
            other => other_packages.push(other.to_string()),
        }
    }
    if !other_packages.is_empty() {
        let specs: Vec<String> = cfg
            .packages
            .iter()
            .filter(|(k, _)| other_packages.contains(k))
            .map(|(k, v)| {
                if v.is_empty() {
                    format!("{k}@latest")
                } else {
                    format!("{k}@{v}")
                }
            })
            .collect();
        env.vars.insert("ACROPOLIS_MISE_PACKAGES".into(), specs.join(" "));
    }
    let mut custom_steps = Vec::new();
    for (name, step) in &cfg.steps {
        let cmds = command_strings(step);
        match name.as_str() {
            "install" => {
                if cmds.iter().any(|c| c != "..." && !INSTALL_COMMANDS.contains(&c.trim())) {
                    unsupported.push(format!("custom install commands ({})", cmds.join("; ")));
                }
            }
            "build" => {
                let parts: Vec<String> = cmds.iter().filter(|c| *c != "...").cloned().collect();
                if !parts.is_empty() {
                    let mut chain = Vec::new();
                    if cmds.iter().any(|c| c == "...")
                        && let Some((existing, _)) = env.config("BUILD_CMD")
                    {
                        chain.push(existing);
                    }
                    chain.extend(parts);
                    env.vars.insert("ACROPOLIS_BUILD_CMD".into(), chain.join(" && "));
                }
                for (k, v) in &step.variables {
                    if !crate::detect::operator_key(k) {
                        env.vars.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            other => custom_steps.push(serde_json::json!({
                "name": other,
                "commands": step.commands,
                "variables": step.variables,
                "secrets": step.secrets,
                "deployOutputs": step.deploy_outputs,
            })),
        }
    }
    if let Some(d) = &cfg.deploy {
        if let Some(s) = &d.start_command {
            env.vars.insert("ACROPOLIS_START_CMD".into(), s.clone());
        }
        let apt: Vec<String> = d.apt_packages.iter().filter(|p| p.as_str() != "...").cloned().collect();
        if !apt.is_empty() {
            env.vars.insert("ACROPOLIS_DEPLOY_APT_PACKAGES".into(), apt.join(" "));
        }
        let inputs: Vec<&Value> = d.inputs.iter().filter(|i| i.is_object()).collect();
        if !inputs.is_empty() {
            env.vars
                .insert("ACROPOLIS_DEPLOY_INPUTS".into(), serde_json::to_string(&inputs)?);
        }
        if !d.paths.is_empty() {
            env.vars.insert("ACROPOLIS_DEPLOY_PATHS".into(), d.paths.join(":"));
        }
        for (k, v) in &d.variables {
            env.vars.insert(format!("ACROPOLIS_DEPLOY_VAR_{k}"), v.clone());
        }
    }
    if !custom_steps.is_empty() {
        env.vars
            .insert("ACROPOLIS_CUSTOM_STEPS".into(), serde_json::to_string(&custom_steps)?);
    }
    if let Some(p) = &cfg.provider {
        env.vars.insert("ACROPOLIS_PROVIDER".into(), p.clone());
    }
    if !cfg.exclude.is_empty() {
        env.vars.insert("ACROPOLIS_EXCLUDE".into(), cfg.exclude.join("\n"));
    }
    if !unsupported.is_empty() {
        bail!(
            "config uses features that are not supported yet: {}",
            unsupported.join(", ")
        );
    }
    Ok(())
}
