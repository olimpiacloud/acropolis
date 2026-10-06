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
    let explicit = env.vars.get("ACRO_CONFIG_FILE").cloned().or_else(|| env.config("CONFIG_FILE").map(|(v, _)| v));
    let path = match &explicit {
        Some(p) => dir.join(p),
        None => {
            let found = ["acro.json", "railpack.json"].iter().map(|f| dir.join(f)).find(|p| p.exists());
            match found {
                Some(p) => p,
                None => return Ok(None),
            }
        }
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading config {}", path.display()))?;
    let value: Value = json5::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))?;
    let cfg: Config = serde_json::from_value(value).with_context(|| format!("{} does not match the config schema", path.display()))?;
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
    step.commands.iter().filter_map(|c| c.as_str().map(|s| s.to_string())).collect()
}

pub fn apply(cfg: &Config, env: &mut Env) -> Result<()> {
    let mut unsupported = Vec::new();
    if !cfg.build_apt_packages.is_empty() {
        unsupported.push(format!("buildAptPackages ({})", cfg.build_apt_packages.join(", ")));
    }
    let mut other_packages = Vec::new();
    for (k, v) in &cfg.packages {
        match k.as_str() {
            "java" => {
                env.vars.insert("ACRO_JAVA_PACKAGE".into(), v.clone());
            }
            "node" | "go" | "python" | "ruby" | "bun" | "deno" | "rust" => {
                env.vars.entry(format!("ACRO_{}_VERSION", k.to_ascii_uppercase())).or_insert_with(|| v.clone());
            }
            other => other_packages.push(other.to_string()),
        }
    }
    if !other_packages.is_empty() {
        unsupported.push(format!("packages ({})", other_packages.join(", ")));
    }
    for (name, step) in &cfg.steps {
        let cmds = command_strings(step);
        match name.as_str() {
            "install" => {
                if cmds.iter().any(|c| c != "..." && !INSTALL_COMMANDS.contains(&c.trim())) {
                    unsupported.push(format!("custom install commands ({})", cmds.join("; ")));
                }
            }
            "build" => {
                let parts: Vec<String> = cmds
                    .iter()
                    .filter(|c| *c != "...")
                    .cloned()
                    .collect();
                if !parts.is_empty() {
                    let mut chain = Vec::new();
                    if cmds.iter().any(|c| c == "...")
                        && let Some((existing, _)) = env.config("BUILD_CMD")
                    {
                        chain.push(existing);
                    }
                    chain.extend(parts);
                    env.vars.insert("ACRO_BUILD_CMD".into(), chain.join(" && "));
                }
                for (k, v) in &step.variables {
                    env.vars.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            other => unsupported.push(format!("custom step {other:?}")),
        }
    }
    if let Some(d) = &cfg.deploy {
        if let Some(s) = &d.start_command {
            env.vars.insert("ACRO_START_CMD".into(), s.clone());
        }
        if !d.apt_packages.is_empty() {
            env.vars.insert("ACRO_DEPLOY_APT_PACKAGES".into(), d.apt_packages.join(" "));
        }
        if d.inputs.iter().any(|i| i.get("image").is_some()) {
            unsupported.push("deploy.inputs from images".into());
        }
        for (k, v) in &d.variables {
            env.vars.insert(format!("ACRO_DEPLOY_VAR_{k}"), v.clone());
        }
    }
    if let Some(p) = &cfg.provider {
        env.vars.insert("ACRO_PROVIDER".into(), p.clone());
    }
    if !cfg.exclude.is_empty() {
        env.vars.insert("ACRO_EXCLUDE".into(), cfg.exclude.join("\n"));
    }
    if !unsupported.is_empty() {
        bail!("config uses features that are not supported yet: {}", unsupported.join(", "));
    }
    Ok(())
}
