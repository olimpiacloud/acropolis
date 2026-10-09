use crate::install::Inside;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

struct Hunk {
    old_start: usize,
    old: Vec<String>,
    new: Vec<String>,
    new_no_eol: bool,
}

struct FilePatch {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
}

fn strip_prefix(p: &str) -> Option<String> {
    let p = p.split('\t').next().unwrap_or(p).trim();
    if p == "/dev/null" {
        return None;
    }
    let p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
    Some(p.to_string())
}

fn parse(diff: &str) -> Result<Vec<FilePatch>> {
    let lines: Vec<&str> = diff.split('\n').collect();
    let mut out: Vec<FilePatch> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(old) = line.strip_prefix("--- ") {
            let new = lines
                .get(i + 1)
                .and_then(|l| l.strip_prefix("+++ "))
                .ok_or_else(|| anyhow!("patch: `---` without `+++` at line {}", i + 1))?;
            out.push(FilePatch {
                old_path: strip_prefix(old),
                new_path: strip_prefix(new),
                hunks: Vec::new(),
            });
            i += 2;
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            let file = out
                .last_mut()
                .ok_or_else(|| anyhow!("patch: hunk before any file header"))?;
            let ranges = rest.split(" @@").next().unwrap_or("");
            let mut parts = ranges.split_whitespace();
            let old_range = parts
                .next()
                .and_then(|r| r.strip_prefix('-'))
                .ok_or_else(|| anyhow!("patch: bad hunk header {line:?}"))?;
            let new_range = parts
                .next()
                .and_then(|r| r.strip_prefix('+'))
                .ok_or_else(|| anyhow!("patch: bad hunk header {line:?}"))?;
            let count = |r: &str| -> Result<(usize, usize)> {
                let mut it = r.split(',');
                let start = it.next().unwrap_or("0").parse::<usize>()?;
                let len = it.next().map(|n| n.parse::<usize>()).transpose()?.unwrap_or(1);
                Ok((start, len))
            };
            let (old_start, old_len) = count(old_range)?;
            let (_, new_len) = count(new_range)?;
            let mut hunk = Hunk {
                old_start,
                old: Vec::new(),
                new: Vec::new(),
                new_no_eol: false,
            };
            i += 1;
            let mut last = ' ';
            while i < lines.len()
                && (hunk.old.len() < old_len || hunk.new.len() < new_len || lines[i].starts_with('\\'))
            {
                let l = lines[i];
                match l.chars().next() {
                    Some(' ') => {
                        hunk.old.push(l[1..].to_string());
                        hunk.new.push(l[1..].to_string());
                        last = ' ';
                    }
                    Some('-') => {
                        hunk.old.push(l[1..].to_string());
                        last = '-';
                    }
                    Some('+') => {
                        hunk.new.push(l[1..].to_string());
                        last = '+';
                    }
                    Some('\\') => {
                        if last != '-' {
                            hunk.new_no_eol = true;
                        }
                    }
                    None => {
                        hunk.old.push(String::new());
                        hunk.new.push(String::new());
                        last = ' ';
                    }
                    _ => bail!("patch: unexpected line {:?} in hunk", l),
                }
                i += 1;
            }
            file.hunks.push(hunk);
            continue;
        }
        i += 1;
    }
    Ok(out)
}

fn find(lines: &[String], want: &[String], hint: usize) -> Option<usize> {
    let fits = |pos: usize| pos + want.len() <= lines.len() && lines[pos..pos + want.len()] == *want;
    if want.is_empty() {
        return Some(hint.min(lines.len()));
    }
    for delta in 0..=lines.len() {
        if hint >= delta && fits(hint - delta) {
            return Some(hint - delta);
        }
        if fits(hint + delta) {
            return Some(hint + delta);
        }
        if hint < delta && hint + delta > lines.len() {
            break;
        }
    }
    None
}

fn checked(inside: &Inside, root: &Path, rel: &str) -> Result<PathBuf> {
    let path = root.join(rel);
    if let Some(parent) = path.parent()
        && parent.exists()
        && !inside.contains(parent)
    {
        bail!("patch: {rel} resolves outside the package directory");
    }
    Ok(path)
}

fn apply_file(inside: &Inside, root: &Path, f: &FilePatch) -> Result<()> {
    for p in [&f.old_path, &f.new_path].into_iter().flatten() {
        if p.starts_with('/') || p.contains('\0') || p.split('/').any(|c| c == "..") {
            bail!("patch touches unsafe path {p:?}");
        }
    }
    let Some(target) = f.new_path.as_ref().or(f.old_path.as_ref()) else {
        return Ok(());
    };
    let path = checked(inside, root, target)?;
    if f.new_path.is_none() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let (source, mode) = match &f.old_path {
        Some(old) => {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(checked(inside, root, old)?)
                .with_context(|| format!("patch: reading {old}"))?;
            let mode = file.metadata()?.permissions().mode() & 0o777;
            let mut text = String::new();
            file.read_to_string(&mut text)
                .with_context(|| format!("patch: reading {old}"))?;
            (text, mode)
        }
        None => (String::new(), 0o644),
    };
    let had_eol = source.ends_with('\n');
    let mut lines: Vec<String> = source.split('\n').map(|s| s.to_string()).collect();
    if had_eol || source.is_empty() {
        lines.pop();
    }
    let mut offset: isize = 0;
    let mut no_eol = !had_eol && !source.is_empty();
    for h in &f.hunks {
        let hint = (h.old_start.max(1) as isize - 1 + offset).max(0) as usize;
        let pos = find(&lines, &h.old, hint)
            .ok_or_else(|| anyhow!("patch: hunk at line {} does not apply to {target}", h.old_start))?;
        lines.splice(pos..pos + h.old.len(), h.new.iter().cloned());
        offset += h.new.len() as isize - h.old.len() as isize;
        if pos + h.new.len() == lines.len() {
            no_eol = h.new_no_eol;
        }
    }
    let mut text = lines.join("\n");
    if !no_eol && !lines.is_empty() {
        text.push('\n');
    }
    if let Some(parent) = path.parent() {
        inside.dir(parent)?;
    }
    if let Some(old) = &f.old_path
        && old != target
    {
        let _ = std::fs::remove_file(root.join(old));
    }
    let _ = std::fs::remove_file(&path);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(&path)
        .with_context(|| format!("patch: writing {target}"))?;
    out.write_all(text.as_bytes())
        .with_context(|| format!("patch: writing {target}"))?;
    Ok(())
}

pub fn apply(diff: &str, root: &Path) -> Result<usize> {
    let files = parse(diff)?;
    let inside = Inside::new(root)?;
    for f in &files {
        apply_file(&inside, root, f)?;
    }
    Ok(files.len())
}

pub fn targets(spec: &str) -> (String, Option<String>) {
    let at = spec.rfind('@').filter(|&i| i > 0);
    match at {
        Some(i) => (spec[..i].to_string(), Some(spec[i + 1..].to_string())),
        None => (spec.to_string(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_git_style_patches() {
        let dir = std::env::temp_dir().join(format!("acropolis-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(
            dir.join("dist/index.js"),
            body.replace("line 20\n", "defaultValue: x,\n"),
        )
        .unwrap();
        let diff = "diff --git a/dist/index.js b/dist/index.js\nindex 1..2 100644\n--- a/dist/index.js\n+++ b/dist/index.js\n@@ -17,7 +17,7 @@ ctx\n line 17\n line 18\n line 19\n-defaultValue: x,\n+value: x ?? \"\",\n line 21\n line 22\n line 23\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n\\ No newline at end of file\n";
        assert_eq!(apply(diff, &dir).unwrap(), 2);
        let out = std::fs::read_to_string(dir.join("dist/index.js")).unwrap();
        assert!(out.contains("value: x ?? \"\",\nline 21"));
        assert!(!out.contains("defaultValue"));
        assert!(out.ends_with("line 30\n"));
        assert_eq!(std::fs::read_to_string(dir.join("new.txt")).unwrap(), "hello");
        assert!(apply("--- a/../x\n+++ b/../x\n@@ -1 +1 @@\n-a\n+b\n", &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stays_inside_package_and_keeps_mode() {
        let base = std::env::temp_dir().join(format!("acropolis-patch-escape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pkg = base.join("pkg");
        let outside = base.join("outside");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "a\n").unwrap();
        std::os::unix::fs::symlink(&outside, pkg.join("evil")).unwrap();
        assert!(apply("--- /dev/null\n+++ b/evil/cron\n@@ -0,0 +1 @@\n+pwned\n", &pkg).is_err());
        assert!(!outside.join("cron").exists());
        assert!(apply("--- a/evil/secret\n+++ b/leak\n@@ -1 +1 @@\n-a\n+a\n", &pkg).is_err());
        assert!(!pkg.join("leak").exists());
        std::fs::write(pkg.join("bin/cli.js"), "old\n").unwrap();
        std::fs::set_permissions(pkg.join("bin/cli.js"), std::fs::Permissions::from_mode(0o755)).unwrap();
        apply("--- a/bin/cli.js\n+++ b/bin/cli.js\n@@ -1 +1 @@\n-old\n+new\n", &pkg).unwrap();
        assert_eq!(std::fs::read_to_string(pkg.join("bin/cli.js")).unwrap(), "new\n");
        assert_eq!(
            std::fs::metadata(pkg.join("bin/cli.js")).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn package_specs() {
        assert_eq!(
            targets("@radix-ui/react-select@2.3.2"),
            ("@radix-ui/react-select".into(), Some("2.3.2".into()))
        );
        assert_eq!(targets("lodash"), ("lodash".into(), None));
        assert_eq!(targets("@scope/pkg"), ("@scope/pkg".into(), None));
    }
}
