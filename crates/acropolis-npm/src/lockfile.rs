use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LockEntry {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub resolved: Option<String>,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub link: bool,
    #[serde(default)]
    pub dev: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub dev_optional: bool,
    #[serde(default)]
    pub in_bundle: bool,
    #[serde(default)]
    pub has_install_script: bool,
    #[serde(default)]
    pub bin: Option<Value>,
    #[serde(default)]
    pub os: Option<Vec<String>>,
    #[serde(default)]
    pub cpu: Option<Vec<String>>,
    #[serde(default)]
    pub libc: Option<Vec<String>>,
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub workspaces: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageLock {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub lockfile_version: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LockEntry>,
    #[serde(default)]
    pub dependencies: Option<BTreeMap<String, V1Dep>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct V1Dep {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub resolved: Option<String>,
    #[serde(default)]
    pub integrity: Option<String>,
    #[serde(default)]
    pub dev: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub bundled: bool,
    #[serde(default)]
    pub dependencies: Option<BTreeMap<String, V1Dep>>,
}

impl PackageLock {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut lock: PackageLock = serde_json::from_slice(data).context("parsing package-lock.json")?;
        if lock.packages.is_empty() {
            if let Some(deps) = lock.dependencies.take() {
                let mut out = BTreeMap::new();
                flatten_v1("node_modules", &deps, &mut out);
                lock.packages = out;
            } else if lock.lockfile_version == 0 {
                bail!("package-lock.json has no lockfileVersion");
            }
        }
        Ok(lock)
    }
}

fn flatten_v1(prefix: &str, deps: &BTreeMap<String, V1Dep>, out: &mut BTreeMap<String, LockEntry>) {
    for (name, d) in deps {
        let path = format!("{prefix}/{name}");
        let version = d.version.clone();
        let is_link = version.as_deref().map(|v| v.starts_with("file:")).unwrap_or(false);
        let resolved = if is_link {
            version.as_deref().map(|v| v.trim_start_matches("file:").to_string())
        } else {
            d.resolved.clone()
        };
        out.insert(
            path.clone(),
            LockEntry {
                name: Some(name.clone()),
                version: if is_link { None } else { version },
                resolved,
                integrity: d.integrity.clone(),
                link: is_link,
                dev: d.dev,
                optional: d.optional,
                in_bundle: d.bundled,
                ..Default::default()
            },
        );
        if let Some(sub) = &d.dependencies {
            flatten_v1(&format!("{path}/node_modules"), sub, out);
        }
    }
}

pub fn package_name_from_path(path: &str) -> &str {
    let idx = path
        .rfind("node_modules/")
        .map(|i| i + "node_modules/".len())
        .unwrap_or(0);
    &path[idx..]
}

/// Byte index of the `@` between a package name and its version; a leading `@` starts a scope.
pub fn version_sep(spec: &str) -> Option<usize> {
    spec.char_indices().skip(1).find(|&(_, c)| c == '@').map(|(i, _)| i)
}

pub fn bins_of(name: &str, bin: &Option<Value>) -> Vec<(String, String)> {
    match bin {
        Some(Value::String(s)) => {
            let short = name.rsplit('/').next().unwrap_or(name).to_string();
            vec![(short, s.clone())]
        }
        Some(Value::Object(m)) => m
            .iter()
            .filter_map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.rsplit('/').next().unwrap_or(k).to_string(), s.to_string()))
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_and_v1() {
        let v3 = br#"{"lockfileVersion":3,"packages":{"":{"name":"x"},"node_modules/a":{"version":"1.0.0","resolved":"https://r/a.tgz","integrity":"sha512-AAAA","bin":{"a":"cli.js"}},"node_modules/a/node_modules/@s/b":{"version":"2.0.0","dev":true}}}"#;
        let l = PackageLock::parse(v3).unwrap();
        assert_eq!(l.packages.len(), 3);
        assert_eq!(package_name_from_path("node_modules/a/node_modules/@s/b"), "@s/b");
        let v1 = br#"{"lockfileVersion":1,"dependencies":{"a":{"version":"1.0.0","resolved":"https://r/a.tgz","integrity":"sha1-x","dependencies":{"b":{"version":"2.0.0"}}},"loc":{"version":"file:../loc"}}}"#;
        let l = PackageLock::parse(v1).unwrap();
        assert!(l.packages.contains_key("node_modules/a/node_modules/b"));
        assert!(l.packages["node_modules/loc"].link);
        assert_eq!(
            bins_of("@s/tool", &Some(Value::String("x.js".into()))),
            vec![("tool".to_string(), "x.js".to_string())]
        );
    }
}
