use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub struct PkgIndex {
    pub files: HashMap<String, Arc<Vec<u8>>>,
    pub dirs: HashSet<String>,
    pub pj: Value,
    materialized: Mutex<HashSet<String>>,
}

pub const EXTENSIONS: &[&str] = &["", ".mjs", ".js", ".jsx", ".ts", ".tsx", ".cjs", ".json"];

impl PkgIndex {
    pub fn new(entries: Vec<acropolis_npm::install::TarEntry>) -> Self {
        let mut files = HashMap::new();
        let mut dirs = HashSet::new();
        dirs.insert(String::new());
        for e in entries {
            if e.kind != acropolis_oci::tar::Kind::File {
                continue;
            }
            let mut acc = String::new();
            let parts: Vec<&str> = e.rel.split('/').collect();
            for p in &parts[..parts.len() - 1] {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(p);
                dirs.insert(acc.clone());
            }
            files.insert(e.rel, Arc::new(e.data));
        }
        let pj = files.get("package.json").and_then(|d| serde_json::from_slice(d).ok()).unwrap_or(Value::Null);
        PkgIndex { files, dirs, pj, materialized: Mutex::new(HashSet::new()) }
    }

    pub fn materialize(&self, pkg_root: &Path, rel: &str) -> std::io::Result<()> {
        let mut done = self.materialized.lock().unwrap();
        if done.contains(rel) {
            return Ok(());
        }
        let Some(data) = self.files.get(rel) else { return Ok(()) };
        let dest = pkg_root.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension(format!("acropolis-tmp-{}", std::process::id()));
        std::fs::write(&tmp, data.as_slice())?;
        std::fs::rename(&tmp, &dest)?;
        done.insert(rel.to_string());
        Ok(())
    }

    pub fn materialize_package_jsons(&self, pkg_root: &Path, rel_file: &str) -> std::io::Result<()> {
        self.materialize(pkg_root, "package.json")?;
        let mut dir = Path::new(rel_file).parent();
        while let Some(d) = dir {
            let s = d.to_string_lossy();
            if s.is_empty() {
                break;
            }
            let pj = format!("{s}/package.json");
            if self.files.contains_key(&pj) {
                self.materialize(pkg_root, &pj)?;
            }
            dir = d.parent();
        }
        Ok(())
    }

    fn file(&self, rel: &str) -> Option<String> {
        for ext in EXTENSIONS {
            let c = format!("{rel}{ext}");
            if self.files.contains_key(&c) {
                return Some(c);
            }
        }
        None
    }

    pub fn resolve_path(&self, rel: &str) -> Option<String> {
        let rel = normalize(rel)?;
        if let Some(f) = self.file(&rel) {
            return Some(f);
        }
        if self.dirs.contains(&rel) {
            let pj_rel = if rel.is_empty() { "package.json".to_string() } else { format!("{rel}/package.json") };
            if let Some(data) = self.files.get(&pj_rel)
                && let Ok(v) = serde_json::from_slice::<Value>(data)
            {
                for field in ["module", "main"] {
                    if let Some(m) = v.get(field).and_then(|m| m.as_str()) {
                        let target = if rel.is_empty() { m.to_string() } else { format!("{rel}/{m}") };
                        if let Some(f) = self.resolve_path(&target) {
                            return Some(f);
                        }
                    }
                }
            }
            let idx = if rel.is_empty() { "index".to_string() } else { format!("{rel}/index") };
            return self.file(&idx);
        }
        None
    }

    pub fn resolve_subpath(&self, subpath: &str, conditions: &[&str]) -> Option<String> {
        if let Some(exports) = self.pj.get("exports").filter(|e| !e.is_null()) {
            let key = if subpath.is_empty() { ".".to_string() } else { format!("./{subpath}") };
            let target = resolve_exports(exports, &key, conditions)?;
            return self.resolve_path(target.trim_start_matches("./"));
        }
        if subpath.is_empty() {
            if let Some(b) = self.pj.get("browser").and_then(|b| b.as_str())
                && let Some(f) = self.resolve_path(b.trim_start_matches("./"))
            {
                return Some(f);
            }
            for field in ["module", "main"] {
                if let Some(m) = self.pj.get(field).and_then(|m| m.as_str())
                    && let Some(f) = self.resolve_path(m.trim_start_matches("./"))
                {
                    return Some(f);
                }
            }
            return self.file("index");
        }
        self.resolve_path(subpath)
    }

    pub fn browser_remaps(&self) -> bool {
        self.pj.get("browser").map(|b| b.is_object()).unwrap_or(false)
    }

    pub fn side_effects(&self, rel: &str) -> Option<bool> {
        match self.pj.get("sideEffects")? {
            Value::Bool(b) => Some(*b),
            Value::Array(globs) => {
                let hit = globs.iter().filter_map(|g| g.as_str()).any(|g| {
                    let g = g.trim_start_matches("./");
                    let pat = if g.contains('/') { g.to_string() } else { format!("**/{g}") };
                    glob_path(&pat, rel)
                });
                Some(hit)
            }
            _ => None,
        }
    }
}

fn glob_path(pat: &str, path: &str) -> bool {
    let p: Vec<&str> = pat.split('/').collect();
    let s: Vec<&str> = path.split('/').collect();
    fn rec(p: &[&str], s: &[&str]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p[0] == "**" {
            return rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..]));
        }
        !s.is_empty() && seg(p[0].as_bytes(), s[0].as_bytes()) && rec(&p[1..], &s[1..])
    }
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => seg(&p[1..], s) || (!s.is_empty() && seg(p, &s[1..])),
            (Some(a), Some(b)) if a == b || *a == b'?' => seg(&p[1..], &s[1..]),
            _ => false,
        }
    }
    rec(&p, &s)
}

pub fn normalize(rel: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    for c in rel.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            other => out.push(other),
        }
    }
    Some(out.join("/"))
}

fn pick_target(target: &Value, star: Option<&str>, conditions: &[&str]) -> Option<String> {
    match target {
        Value::String(s) => Some(match star {
            Some(m) => s.replace('*', m),
            None => s.clone(),
        }),
        Value::Array(items) => items.iter().find_map(|t| pick_target(t, star, conditions)),
        Value::Object(map) => {
            for (k, v) in map {
                if k == "default" || conditions.contains(&k.as_str()) {
                    if let Some(r) = pick_target(v, star, conditions) {
                        return Some(r);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

pub fn resolve_exports(exports: &Value, key: &str, conditions: &[&str]) -> Option<String> {
    let is_subpath_map = matches!(exports, Value::Object(m) if m.keys().next().map(|k| k.starts_with('.')).unwrap_or(false));
    if !is_subpath_map {
        return if key == "." { pick_target(exports, None, conditions) } else { None };
    }
    let map = exports.as_object()?;
    if let Some(t) = map.get(key) {
        return pick_target(t, None, conditions);
    }
    let mut best: Option<(&str, String)> = None;
    for (k, v) in map {
        if let Some(star) = k.find('*') {
            let (prefix, suffix) = (&k[..star], &k[star + 1..]);
            if key.starts_with(prefix) && key.ends_with(suffix) && key.len() >= prefix.len() + suffix.len() {
                let m = &key[prefix.len()..key.len() - suffix.len()];
                if best.as_ref().map(|(bk, _)| prefix.len() > bk.len()).unwrap_or(true)
                    && let Some(t) = pick_target(v, Some(m), conditions)
                {
                    best = Some((prefix, t));
                }
            } else if k.ends_with('/') && key.starts_with(k.as_str()) {
                let rest = &key[k.len()..];
                if let Some(t) = pick_target(v, None, conditions) {
                    best = Some((k.as_str(), format!("{t}{rest}")));
                }
            }
        } else if k.ends_with('/') && key.starts_with(k.as_str()) {
            let rest = &key[k.len()..];
            if let Some(t) = pick_target(v, None, conditions) {
                best = Some((k.as_str(), format!("{t}{rest}")));
            }
        }
    }
    best.map(|(_, t)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exports_resolution() {
        let c = ["import", "module", "browser", "production", "default"];
        let e = json!({".": {"types": "./x.d.ts", "import": "./esm/index.js", "require": "./cjs/index.js"}, "./*": {"import": "./esm/*/index.js", "require": "./cjs/*/index.js"}, "./package.json": "./package.json"});
        assert_eq!(resolve_exports(&e, ".", &c).unwrap(), "./esm/index.js");
        assert_eq!(resolve_exports(&e, "./Button", &c).unwrap(), "./esm/Button/index.js");
        let r = ["require", "default"];
        assert_eq!(resolve_exports(&e, ".", &r).unwrap(), "./cjs/index.js");
        assert_eq!(resolve_exports(&json!("./main.js"), ".", &c).unwrap(), "./main.js");
        assert!(resolve_exports(&json!({".": "./a.js"}), "./b", &c).is_none());
        assert!(glob_path("**/*.css", "esm/a/b.css"));
        assert_eq!(normalize("a/./b/../c").unwrap(), "a/c");
    }
}
