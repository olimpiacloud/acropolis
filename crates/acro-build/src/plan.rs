use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Action {
    ResolveBase { image: String },
    ResolveNodeBase { spec: String, variant: String },
    CopyBase,
    Toolchain { tool: String, spec: String, parts: Vec<String> },
    NpmFetch { lockfile: String, lockfile_sha256: String, dev: bool },
    NpmInstall { dev: bool },
    GoModules { gosum_sha256: String },
    CopySource { exclude: Vec<String> },
    Run { argv: Vec<String>, env: BTreeMap<String, String>, network: bool, cwd: String },
    Layer { dest: String, from: LayerFrom },
    Push,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum LayerFrom {
    AppSource { exclude: Vec<String> },
    NodeModules { dev: bool },
    WorkDir { path: String, exclude: Vec<String> },
    WorkFile { path: String, mode: u32 },
    Paths { items: Vec<(String, String)> },
    Inline { files: BTreeMap<String, String> },
}

impl Action {
    pub fn class(&self) -> &'static str {
        match self {
            Action::ResolveBase { .. } | Action::ResolveNodeBase { .. } | Action::Toolchain { .. } | Action::NpmFetch { .. } | Action::GoModules { .. } => {
                "fetch"
            }
            Action::Run { network: true, .. } => "build+net",
            Action::Run { .. } => "build",
            Action::CopyBase | Action::Push => "push",
            _ => "local",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub name: String,
    pub action: Action,
    pub deps: Vec<String>,
    #[serde(default)]
    pub hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ImageSpec {
    pub layers: Vec<String>,
    pub workdir: Option<String>,
    pub env: Vec<(String, String)>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub ports: Vec<u16>,
    pub user: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub app: String,
    pub provider: String,
    pub facts: BTreeMap<String, String>,
    pub steps: Vec<Step>,
    pub image: ImageSpec,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub hash: String,
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> =
                keys.iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical(&m[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

pub fn hash_value(v: &Value) -> String {
    acro_store::sha256_bytes(canonical(v).as_bytes()).hex()
}

impl Plan {
    pub fn finalize(&mut self) {
        let mut hashes: BTreeMap<String, String> = BTreeMap::new();
        for s in self.steps.iter_mut() {
            let deps: Vec<String> = s.deps.iter().map(|d| hashes.get(d).cloned().unwrap_or_default()).collect();
            let v = serde_json::json!({ "action": serde_json::to_value(&s.action).unwrap(), "deps": deps });
            s.hash = hash_value(&v);
            hashes.insert(s.id.clone(), s.hash.clone());
        }
        let v = serde_json::json!({
            "steps": self.steps.iter().map(|s| s.hash.clone()).collect::<Vec<_>>(),
            "image": serde_json::to_value(&self.image).unwrap(),
        });
        self.hash = hash_value(&v);
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("plan {} for {} ({})\n", &self.hash[..12], self.app, self.provider));
        for (k, v) in &self.facts {
            out.push_str(&format!("  {k:<16} {v}\n"));
        }
        out.push_str("\nsteps:\n");
        for s in &self.steps {
            let deps = if s.deps.is_empty() { String::new() } else { format!("  <- {}", s.deps.join(", ")) };
            out.push_str(&format!(
                "  {:<14} [{:<9}] {}  #{}{}\n",
                s.id,
                s.action.class(),
                s.name,
                &s.hash[..10],
                deps
            ));
        }
        out.push_str("\nimage:\n");
        out.push_str(&format!("  layers   {}\n", self.image.layers.join(", ")));
        if let Some(w) = &self.image.workdir {
            out.push_str(&format!("  workdir  {w}\n"));
        }
        for (k, v) in &self.image.env {
            out.push_str(&format!("  env      {k}={v}\n"));
        }
        if let Some(c) = &self.image.cmd {
            out.push_str(&format!("  cmd      {}\n", serde_json::to_string(c).unwrap()));
        }
        for w in &self.warnings {
            out.push_str(&format!("\nwarning: {w}\n"));
        }
        out
    }
}

pub struct PlanBuilder {
    pub plan: Plan,
}

impl PlanBuilder {
    pub fn new(app: &str, provider: &str) -> Self {
        PlanBuilder {
            plan: Plan {
                app: app.to_string(),
                provider: provider.to_string(),
                facts: BTreeMap::new(),
                steps: Vec::new(),
                image: ImageSpec::default(),
                warnings: Vec::new(),
                hash: String::new(),
            },
        }
    }

    pub fn fact(&mut self, k: &str, v: impl Into<String>) -> &mut Self {
        self.plan.facts.insert(k.to_string(), v.into());
        self
    }

    pub fn step(&mut self, id: &str, name: impl Into<String>, action: Action, deps: &[&str]) -> String {
        self.plan.steps.push(Step {
            id: id.to_string(),
            name: name.into(),
            action,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            hash: String::new(),
        });
        id.to_string()
    }

    pub fn finish(mut self) -> Plan {
        self.plan.finalize();
        self.plan
    }
}
