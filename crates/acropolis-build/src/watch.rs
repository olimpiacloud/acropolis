//! Repo paths whose changes can change the image (`watchPaths` in info.json), relative to the workspace root.
//!
//! Only a node workspace member gets a narrowed list: its own dir, the members it reaches through any dependency
//! kind (devDependencies too: they take part in the build), and the root files that drive the install and the
//! toolchain. Anything Acropolis cannot follow (scripts or configs that reach `../`, task runners that build other
//! members, `file:` dependencies or tsconfig `extends` outside the root) falls back to `["."]`.

use crate::detect::{read_app_file, read_package_json, workspace_members, workspace_root};
use crate::plan::{Action, Plan};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Files and dirs at the workspace root that the install or the toolchain read for a member build.
const ROOT_INPUTS: &[&str] = &[
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "bun.lock",
    "bun.lockb",
    "bunfig.toml",
    "yarn.lock",
    ".yarnrc",
    ".yarnrc.yml",
    ".yarn",
    ".npmrc",
    ".pnpmfile.cjs",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    "mise.toml",
    ".mise.toml",
    "mise.lock",
    "mise",
    ".config/mise.toml",
    ".dockerignore",
];

/// Commands that build or read other members than the ones the dependency graph names.
const ESCAPES: &[&str] = &[
    "../",
    "turbo",
    "nx ",
    "lerna",
    "--filter",
    "-F ",
    "--workspace",
    "--cwd",
    "--dir",
    "--prefix",
    "-C ",
];

pub fn watch_paths(dir: &Path, plan: &Plan) -> Vec<String> {
    member_paths(dir, plan).unwrap_or_else(|| vec![".".to_string()])
}

fn member_paths(dir: &Path, plan: &Plan) -> Option<Vec<String>> {
    if plan.provider != "node" || plan.facts.contains_key("nx-app") {
        return None;
    }
    let rel = plan.facts.get("workspace-member")?;
    let (root, found) = workspace_root(dir)?;
    if found != *rel {
        return None;
    }
    for s in &plan.steps {
        let text = match &s.action {
            Action::Run { argv, .. } => argv.join(" "),
            Action::ImageRun { commands, .. } => commands.join("\n"),
            _ => continue,
        };
        if escapes(&text) {
            return None;
        }
    }
    let root_pj = read_package_json(&root).unwrap_or(Value::Null);
    // Root install hooks run with the whole tree in reach.
    if ["preinstall", "install", "postinstall", "prepare"]
        .iter()
        .any(|k| root_pj.get("scripts").and_then(|s| s.get(*k)).is_some())
    {
        return None;
    }
    if build_scripts(&read_package_json(&root.join(rel)).ok()?)
        .iter()
        .any(|s| escapes(s))
    {
        return None;
    }
    let all_kinds = [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ];
    let mut members = vec![rel.clone()];
    members.extend(crate::providers::node::workspace_deps(&root, rel, &all_kinds));
    // The root's own dependencies are installed with the member, workspace links included.
    for m in crate::providers::node::workspace_deps(&root, "", &all_kinds) {
        members.extend(crate::providers::node::workspace_deps(&root, &m, &all_kinds));
        members.push(m);
    }
    let names: Vec<(String, String)> = workspace_members(&root, &root_pj)
        .into_iter()
        .filter_map(|m| {
            Some((
                read_package_json(&root.join(&m))
                    .ok()?
                    .get("name")?
                    .as_str()?
                    .to_string(),
                m,
            ))
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < members.len() {
        let m = members[i].clone();
        i += 1;
        if out.contains(&m) {
            continue;
        }
        let mdir = root.join(&m);
        let pj = read_package_json(&mdir).ok()?;
        for key in ["dependencies", "devDependencies", "optionalDependencies"] {
            for spec in pj
                .get(key)
                .and_then(|d| d.as_object())
                .into_iter()
                .flatten()
                .filter_map(|(_, v)| v.as_str())
            {
                if let Some(p) = spec.strip_prefix("file:").or_else(|| spec.strip_prefix("link:")) {
                    out.push(inside(&root, &mdir, p)?);
                }
            }
        }
        for (target, from_name) in tsconfig_refs(&mdir) {
            if from_name {
                // `extends: "@scope/pkg/base.json"`: a member hoisted to the root's node_modules.
                if let Some((_, path)) = names
                    .iter()
                    .find(|(n, _)| target == *n || target.starts_with(&format!("{n}/")))
                {
                    members.push(path.clone());
                }
            } else {
                out.push(inside(&root, &mdir, &target)?);
            }
        }
        if config_escapes(&mdir) {
            return None;
        }
        out.push(m);
    }
    for f in ROOT_INPUTS {
        if root.join(f).exists() {
            out.push(f.to_string());
        }
    }
    for (_, patch) in crate::run::patched_dependencies(&root) {
        out.push(inside(&root, &root, &patch)?);
    }
    Some(minimal(out))
}

fn escapes(cmd: &str) -> bool {
    ESCAPES.iter().any(|e| cmd.contains(e))
}

/// Script bodies the build can run: `build`, its pre/post hooks and every script they `run`, transitively.
fn build_scripts(pj: &Value) -> Vec<String> {
    let scripts = pj.get("scripts").and_then(|s| s.as_object());
    let get = |n: &str| {
        scripts
            .and_then(|s| s.get(n))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let mut names = vec!["prebuild".to_string(), "build".to_string(), "postbuild".to_string()];
    let mut out = Vec::new();
    let mut i = 0;
    while i < names.len() {
        let Some(body) = get(&names[i]) else {
            i += 1;
            continue;
        };
        let words: Vec<&str> = body.split_whitespace().collect();
        for w in words.windows(2).filter(|w| w[0] == "run") {
            let n = w[1].trim_matches(['"', '\'', ';', '&', '|']).to_string();
            if !names.contains(&n) {
                names.push(n);
            }
        }
        out.push(body);
        i += 1;
    }
    out
}

/// `path` (relative to `from`) as a root-relative path, or None when it leaves the root.
fn inside(root: &Path, from: &Path, path: &str) -> Option<String> {
    if Path::new(path).is_absolute() {
        return None;
    }
    let rel_from = from.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = Vec::new();
    for c in rel_from.join(path).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(if parts.is_empty() { ".".into() } else { parts.join("/") })
}

/// `extends` and `references` of the dir's top-level tsconfig files: (target, is a package name).
fn tsconfig_refs(dir: &Path) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("tsconfig") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    for f in files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let Some(v) = read_app_file(dir, name).and_then(|t| json5::from_str::<Value>(&t).ok()) else {
            continue;
        };
        let mut targets: Vec<String> = match v.get("extends") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a.iter().filter_map(|s| s.as_str().map(String::from)).collect(),
            _ => vec![],
        };
        targets.extend(
            v.get("references")
                .and_then(|r| r.as_array())
                .into_iter()
                .flatten()
                .filter_map(|r| r.get("path")?.as_str().map(String::from)),
        );
        // `paths` aliases (`"@shared/*": ["../../shared/*"]`) resolve from `baseUrl`, or the tsconfig's dir.
        let opts = v.get("compilerOptions");
        let base = opts
            .and_then(|o| o.get("baseUrl"))
            .and_then(|b| b.as_str())
            .unwrap_or(".");
        for alias in opts
            .and_then(|o| o.get("paths"))
            .and_then(|p| p.as_object())
            .into_iter()
            .flat_map(|p| p.values())
            .filter_map(|v| v.as_array())
            .flatten()
            .filter_map(|s| s.as_str())
        {
            let dir = alias.split('*').next().unwrap_or("").trim_end_matches('/');
            // `inside` normalizes it and decides: a target in the member is covered by the member itself, one out of
            // the root makes the whole list `["."]`.
            let joined = Path::new(base).join(dir);
            let joined = joined.to_string_lossy();
            out.push((
                if joined.starts_with('/') {
                    joined.into_owned()
                } else {
                    format!("./{joined}")
                },
                false,
            ));
        }
        for t in targets {
            let relative = t.starts_with("./") || t.starts_with("../") || t == "." || t == "..";
            out.push((t, !relative));
        }
    }
    out
}

/// A bundler or framework config at the member's top level that imports from outside the member.
fn config_escapes(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_config = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts"]
            .iter()
            .any(|ext| name.contains(".config.") && name.ends_with(ext));
        is_config
            && read_app_file(dir, &name)
                .is_some_and(|t| t.contains("\"../") || t.contains("'../") || t.contains("`../"))
    })
}

/// Sorted, without entries another entry already covers.
fn minimal(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    if paths.iter().any(|p| p == ".") {
        return vec![".".into()];
    }
    let all = paths.clone();
    paths.retain(|p| {
        !all.iter()
            .any(|q| q != p && p.starts_with(q.as_str()) && p.as_bytes().get(q.len()) == Some(&b'/'))
    });
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/suites/workspaces")
            .join(name)
    }

    fn plan_paths(dir: &Path) -> Vec<String> {
        let plan = crate::plan_app(dir, &crate::Env::default()).unwrap();
        watch_paths(dir, &plan)
    }

    #[test]
    fn member_watches_its_dependencies_dev_ones_included() {
        let root = fixture("bun-member-dev-tsconfig");
        assert_eq!(
            plan_paths(&root.join("apps/app")),
            [
                "apps/app",
                "bun.lock",
                "package.json",
                "packages/a",
                "packages/tsconfig"
            ]
        );
        let root = fixture("pnpm-member-dev-tsconfig");
        assert_eq!(
            plan_paths(&root.join("apps/app")),
            [
                "apps/app",
                "package.json",
                "packages/a",
                "packages/tsconfig",
                "pnpm-lock.yaml",
                "pnpm-workspace.yaml"
            ]
        );
    }

    #[test]
    fn repo_without_workspace_watches_everything() {
        let dir = crate::detect::scratch_dir("watch-single");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name": "single", "scripts": {"start": "node index.js"}}"#,
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "console.log(1)\n").unwrap();
        assert_eq!(plan_paths(&dir), ["."]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn member_reaching_outside_its_dependencies_watches_everything() {
        let base = crate::detect::scratch_dir("watch-escape");
        for d in ["apps/web", "packages/ui", "shared"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::write(base.join("package.json"), r#"{"workspaces": ["apps/*", "packages/*"]}"#).unwrap();
        std::fs::write(base.join("packages/ui/package.json"), r#"{"name": "ui"}"#).unwrap();
        let web = |pj: &str, tsconfig: &str| {
            std::fs::write(base.join("apps/web/package.json"), pj).unwrap();
            std::fs::write(base.join("apps/web/tsconfig.json"), tsconfig).unwrap();
            plan_paths(&base.join("apps/web"))
        };
        let pj = r#"{"name": "web", "dependencies": {"ui": "*"}, "scripts": {"build": "tsc", "start": "node dist/index.js"}}"#;
        assert_eq!(web(pj, "{}"), ["apps/web", "package.json", "packages/ui"]);
        // A relative `extends` (with a comment, as tsconfig allows) adds the file it names.
        assert_eq!(
            web(
                pj,
                "{\n  // shared options\n  \"extends\": \"../../tsconfig.base.json\"\n}"
            ),
            ["apps/web", "package.json", "packages/ui", "tsconfig.base.json"]
        );
        assert_eq!(web(pj, r#"{"extends": "../../../elsewhere.json"}"#), ["."]);
        assert_eq!(
            web(
                pj,
                r#"{"compilerOptions": {"paths": {"@shared/*": ["../../shared/*"]}}}"#
            ),
            ["apps/web", "package.json", "packages/ui", "shared"]
        );
        assert_eq!(
            web(
                pj,
                r#"{"compilerOptions": {"paths": {"@shared/*": ["./../../shared/*"], "@/*": ["./src/*"]}}}"#
            ),
            ["apps/web", "package.json", "packages/ui", "shared"]
        );
        assert_eq!(
            web(
                pj,
                r#"{"compilerOptions": {"baseUrl": "src", "paths": {"@x/*": ["../../../../x/*"]}}}"#
            ),
            ["."]
        );
        let pj = r#"{"name": "web", "scripts": {"build": "cp -r ../../shared dist", "start": "node dist/index.js"}}"#;
        assert_eq!(web(pj, "{}"), ["."]);
        let pj = r#"{"name": "web", "scripts": {"build": "turbo run build", "start": "node dist/index.js"}}"#;
        assert_eq!(web(pj, "{}"), ["."]);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
