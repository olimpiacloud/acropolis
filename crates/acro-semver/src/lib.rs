use semver::{Version, VersionReq};

pub fn parse_loose_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('=');
    if let Ok(v) = Version::parse(s) {
        return Some(v);
    }
    let parts: Vec<&str> = s.split('.').collect();
    let nums: Vec<u64> = parts.iter().map_while(|p| p.parse().ok()).collect();
    match nums.len() {
        1 => Some(Version::new(nums[0], 0, 0)),
        2 => Some(Version::new(nums[0], nums[1], 0)),
        _ => None,
    }
}

pub fn is_exact(s: &str) -> bool {
    let s = s.trim().trim_start_matches('v');
    Version::parse(s).is_ok()
}

fn normalize_comparator(c: &str) -> Option<String> {
    let c = c.trim();
    if c.is_empty() || c == "*" || c == "x" || c == "X" || c == "latest" {
        return Some("*".into());
    }
    let (op, rest) = if let Some(r) = c.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = c.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = c.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = c.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = c.strip_prefix('^') {
        ("^", r)
    } else if let Some(r) = c.strip_prefix('~') {
        ("~", r.trim_start_matches('>'))
    } else if let Some(r) = c.strip_prefix('=') {
        ("=", r)
    } else {
        ("", c)
    };
    let rest = rest.trim().trim_start_matches('v');
    let parts: Vec<&str> = rest.split('.').filter(|p| !matches!(*p, "x" | "X" | "*")).collect();
    if parts.is_empty() {
        return Some("*".into());
    }
    let ver = parts.join(".");
    let op = if op.is_empty() || op == "=" {
        if parts.len() < 3 { "" } else { "=" }
    } else {
        op
    };
    if op.is_empty() {
        return Some(format!("{ver}.*").replace(".*.*", ".*"));
    }
    Some(format!("{op}{ver}"))
}

pub fn parse_range(spec: &str) -> Vec<VersionReq> {
    let mut out = Vec::new();
    for alt in spec.split("||") {
        let alt = alt.trim();
        let comparators: Vec<String> = if let Some((lo, hi)) = alt.split_once(" - ") {
            vec![format!(">={}", lo.trim()), format!("<={}", hi.trim())]
        } else {
            let mut toks: Vec<String> = Vec::new();
            let mut pending_op = String::new();
            for t in alt.split_whitespace() {
                if matches!(t, ">=" | "<=" | ">" | "<" | "=" | "^" | "~") {
                    pending_op = t.to_string();
                    continue;
                }
                toks.push(format!("{pending_op}{t}"));
                pending_op.clear();
            }
            toks
        };
        let norm: Vec<String> = comparators.iter().filter_map(|c| normalize_comparator(c)).collect();
        let joined = if norm.is_empty() { "*".to_string() } else { norm.join(", ") };
        if let Ok(r) = VersionReq::parse(&joined) {
            out.push(r);
        }
    }
    out
}

pub fn best_match<'a>(spec: &str, versions: impl Iterator<Item = &'a Version>) -> Option<Version> {
    let reqs = parse_range(spec);
    if reqs.is_empty() {
        return None;
    }
    versions.filter(|v| v.pre.is_empty() && reqs.iter().any(|r| r.matches(v))).max().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let vs: Vec<Version> = ["18.20.5", "20.11.0", "20.18.1", "22.11.0", "22.20.0", "23.5.0"]
            .iter()
            .map(|s| Version::parse(s).unwrap())
            .collect();
        assert_eq!(best_match("22", vs.iter()).unwrap().to_string(), "22.20.0");
        assert_eq!(best_match(">=18 <21", vs.iter()).unwrap().to_string(), "20.18.1");
        assert_eq!(best_match("^20.11", vs.iter()).unwrap().to_string(), "20.18.1");
        assert_eq!(best_match("20.x", vs.iter()).unwrap().to_string(), "20.18.1");
        assert_eq!(best_match(">= 18", vs.iter()).unwrap().to_string(), "23.5.0");
        assert_eq!(best_match("18 || 20", vs.iter()).unwrap().to_string(), "20.18.1");
        assert_eq!(best_match("22.11.0", vs.iter()).unwrap().to_string(), "22.11.0");
        assert_eq!(best_match("~22.11", vs.iter()).unwrap().to_string(), "22.11.0");
        assert!(is_exact("v23.5.0"));
    }
}

pub fn fuzzy_version(spec: &str) -> String {
    let v = spec.trim();
    if v.is_empty() || v == "*" {
        return "latest".into();
    }
    if v.contains(">=") || v.contains('<') {
        let parts: Vec<&str> = v.split_whitespace().collect();
        for (i, part) in parts.iter().enumerate() {
            if let Some(after) = part.strip_prefix(">=") {
                let x = if after.is_empty() { parts.get(i + 1).copied().unwrap_or("") } else { after };
                return x.trim().trim_start_matches('v').split('.').next().unwrap_or("").to_string();
            }
        }
    }
    if let Some(after) = v.strip_prefix('^') {
        return after.trim_start_matches('v').split('.').next().unwrap_or("").to_string();
    }
    let v = v.trim_start_matches('~').trim_start_matches('v');
    let v = v.replace(".x", "");
    v.trim_end_matches('.').to_string()
}

#[cfg(test)]
mod fuzzy_tests {
    use super::fuzzy_version;

    #[test]
    fn railpack_compatible() {
        assert_eq!(fuzzy_version(">=20.0.0"), "20");
        assert_eq!(fuzzy_version(">= 18"), "18");
        assert_eq!(fuzzy_version(">=22 <23"), "22");
        assert_eq!(fuzzy_version("^18.2.0"), "18");
        assert_eq!(fuzzy_version("~22.1"), "22.1");
        assert_eq!(fuzzy_version("20.x"), "20");
        assert_eq!(fuzzy_version("v23.5.0"), "23.5.0");
        assert_eq!(fuzzy_version(""), "latest");
    }
}
