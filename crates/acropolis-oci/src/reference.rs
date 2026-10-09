use anyhow::{Result, bail};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty image reference");
        }
        let (rest, digest) = match s.split_once('@') {
            Some((r, d)) => (r, Some(d.to_string())),
            None => (s, None),
        };
        let (name, tag) = match rest.rfind(':') {
            Some(i) if !rest[i + 1..].contains('/') => (&rest[..i], Some(rest[i + 1..].to_string())),
            _ => (rest, None),
        };
        let (registry, repository) = match name.split_once('/') {
            Some((first, remainder)) if first.contains('.') || first.contains(':') || first == "localhost" => {
                (first.to_string(), remainder.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        let tag = if tag.is_none() && digest.is_none() {
            Some("latest".to_string())
        } else {
            tag
        };
        let valid = registry
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
            && repository.split('/').all(valid_path_component)
            && tag.as_deref().is_none_or(valid_tag)
            && digest.as_deref().is_none_or(valid_digest);
        if !valid {
            bail!("invalid image reference {s:?}");
        }
        Ok(Reference {
            registry,
            repository,
            tag,
            digest,
        })
    }

    pub fn api_host(&self) -> &str {
        if self.registry == "docker.io" {
            "registry-1.docker.io"
        } else {
            &self.registry
        }
    }

    pub fn reference(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }

    pub fn with_digest(&self, digest: &str) -> Reference {
        Reference {
            digest: Some(digest.to_string()),
            ..self.clone()
        }
    }

    pub fn insecure(&self) -> bool {
        let host = self.registry.split(':').next().unwrap_or("");
        host == "localhost" || host == "127.0.0.1" || host.ends_with(".localhost") || host == "[::1]"
    }
}

/// `[a-z0-9]+` joined by `.`, `_`, `__` or runs of `-` (OCI distribution spec).
fn valid_path_component(c: &str) -> bool {
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let b = c.as_bytes();
    if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    c.split(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        .all(|sep| sep.is_empty() || sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|b| b == b'-'))
}

fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// Only sha256 can be verified, so any other digest is rejected instead of trusted.
pub fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_refs() {
        let r = Reference::parse("node:22-slim").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/node");
        assert_eq!(r.tag.as_deref(), Some("22-slim"));
        let r = Reference::parse("localhost:5000/acme/app").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "acme/app");
        assert_eq!(r.tag.as_deref(), Some("latest"));
        let r = Reference::parse(&format!("gcr.io/distroless/cc-debian12@{DIGEST}")).unwrap();
        assert_eq!(r.registry, "gcr.io");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest.as_deref(), Some(DIGEST));
        let r = Reference::parse(&format!("user/repo:1.0@{DIGEST}")).unwrap();
        assert_eq!(r.repository, "user/repo");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
        assert_eq!(Reference::parse(&r.to_string()).unwrap(), r);
        assert!(Reference::parse("my_org/my__app.v2/web-app---x:v1.2_rc-3").is_ok());
    }

    #[test]
    fn rejects_references_that_escape_the_registry_api_path() {
        for bad in [
            "denoland/deno:1/../../v2/x",
            "library/../../v2/token",
            "node:22/../x",
            "node:22?x=1",
            "node#frag",
            "Node:22",
            "node:.hidden",
            "node@sha256:abc",
            "node@sha512:0123",
            &format!("node@sha256:../{}", &DIGEST[10..]),
            "evil.com/a@b/c",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
