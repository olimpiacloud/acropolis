use std::path::Path;

#[derive(Clone, Debug)]
struct Rule {
    negate: bool,
    segments: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
    has_negations: bool,
}

pub const ALWAYS: &[&str] = &[".git", ".acropolis"];

impl Ignore {
    pub fn new(patterns: &[String]) -> Self {
        let mut ig = Ignore::default();
        for p in ALWAYS {
            ig.add(p);
        }
        for p in patterns {
            ig.add(p);
        }
        ig
    }

    pub fn load(dir: &Path, extra: &[String]) -> Self {
        let mut pats: Vec<String> = Vec::new();
        if let Ok(text) = std::fs::read_to_string(dir.join(".dockerignore")) {
            pats.extend(text.lines().map(|l| l.to_string()));
        }
        pats.extend(extra.iter().cloned());
        Self::new(&pats)
    }

    pub fn add(&mut self, raw: &str) {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return;
        }
        let (negate, pat) = match line.strip_prefix('!') {
            Some(p) => (true, p.trim()),
            None => (false, line),
        };
        let pat = pat
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/');
        if pat.is_empty() {
            return;
        }
        let mut segments: Vec<String> = pat
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| s.to_string())
            .collect();
        segments.dedup_by(|a, b| a == "**" && b == "**");
        if segments.is_empty() {
            return;
        }
        if negate {
            self.has_negations = true;
        }
        self.rules.push(Rule { negate, segments });
    }

    pub fn has_negations(&self) -> bool {
        self.has_negations
    }

    pub fn excluded(&self, rel: &str) -> bool {
        let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        let mut excluded = false;
        for r in &self.rules {
            if r.negate == excluded && matches_prefix(&r.segments, &parts) {
                excluded = !r.negate;
            }
        }
        excluded
    }
}

/// True when `pat` matches some leading part of `path` (at least one segment). Simulates the
/// pattern as an NFA over its segments, so `**` costs O(pattern × path) glob calls, not backtracking.
fn matches_prefix(pat: &[String], path: &[&str]) -> bool {
    let n = pat.len();
    let close = |states: &mut [bool]| {
        for i in 0..n {
            if states[i] && pat[i] == "**" {
                states[i + 1] = true;
            }
        }
    };
    // Two state rows; on the stack for any realistic pattern (called per rule per walked entry).
    let mut stack = [false; 64];
    let mut heap = Vec::new();
    let rows = if 2 * (n + 1) <= stack.len() {
        &mut stack[..2 * (n + 1)]
    } else {
        heap.resize(2 * (n + 1), false);
        &mut heap[..]
    };
    let (mut cur, mut next) = rows.split_at_mut(n + 1);
    cur[0] = true;
    close(cur);
    for seg in path {
        next.fill(false);
        for i in (0..n).filter(|&i| cur[i]) {
            if pat[i] == "**" {
                next[i] = true;
            } else if glob(pat[i].as_bytes(), seg.as_bytes()) {
                next[i + 1] = true;
            }
        }
        close(next);
        if next[n] {
            return true;
        }
        if !next.contains(&true) {
            return false;
        }
        std::mem::swap(&mut cur, &mut next);
    }
    false
}

pub fn glob(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star_p, mut star_s) = (usize::MAX, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star_p = pi;
                    star_s = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, next)) = class(&p[pi..], s[si]) {
                        if ok {
                            pi += next;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == s[si] {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c => {
                    if c == s[si] {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
            }
        }
        if star_p != usize::MAX {
            pi = star_p + 1;
            star_s += 1;
            si = star_s;
            continue;
        }
        return false;
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = i < p.len() && (p[i] == b'^' || p[i] == b'!');
    if negate {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_semantics() {
        let ig = Ignore::new(&[
            "node_modules".into(),
            "*.log".into(),
            "**/secret.txt".into(),
            "build/".into(),
            "docs/*".into(),
            "!docs/keep.md".into(),
        ]);
        assert!(ig.excluded("node_modules"));
        assert!(ig.excluded("node_modules/a/b.js"));
        assert!(ig.excluded("debug.log"));
        assert!(!ig.excluded("logs/debug.log"));
        assert!(ig.excluded("a/b/secret.txt"));
        assert!(ig.excluded("secret.txt"));
        assert!(ig.excluded("build/x"));
        assert!(ig.excluded("docs/a.md"));
        assert!(!ig.excluded("docs/keep.md"));
        assert!(ig.excluded(".git/config"));
        assert!(!ig.excluded("src/index.js"));
        let stars = Ignore::new(&[format!("{}x", "**/".repeat(40))]);
        let deep = ["a"; 40].join("/");
        assert!(!stars.excluded(&deep));
        assert!(stars.excluded(&format!("{deep}/x")));
    }

    #[test]
    fn alternating_double_stars_stay_linear() {
        let mid = Ignore::new(&["a/**/b".into(), "**/c/**/c/d".into()]);
        assert!(mid.excluded("a/b"));
        assert!(mid.excluded("a/x/y/b"));
        assert!(mid.excluded("a/b/inner.js"));
        assert!(!mid.excluded("x/a/b"));
        assert!(mid.excluded("x/c/y/c/d/e"));
        assert!(mid.excluded("c/c/d"));
        assert!(!mid.excluded("c/d"));
        let evil = Ignore::new(&[format!("{}b", "**/a/".repeat(30))]);
        let deep = ["a"; 80].join("/");
        let start = std::time::Instant::now();
        assert!(!evil.excluded(&deep));
        assert!(evil.excluded(&format!("{deep}/b")));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn globs() {
        assert!(glob(b"*.js", b"a.js"));
        assert!(!glob(b"*.js", b"a.ts"));
        assert!(glob(b"a?c", b"abc"));
        assert!(glob(b"[a-c]x", b"bx"));
        assert!(!glob(b"[!a-c]x", b"bx"));
    }
}
