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
            Some((first, remainder))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_string(), remainder.to_string())
            }
            _ => ("docker.io".to_string(), name.to_string()),
        };
        let repository = if registry == "docker.io" && !repository.contains('/') {
            format!("library/{repository}")
        } else {
            repository
        };
        let tag = if tag.is_none() && digest.is_none() { Some("latest".to_string()) } else { tag };
        Ok(Reference { registry, repository, tag, digest })
    }

    pub fn api_host(&self) -> &str {
        if self.registry == "docker.io" { "registry-1.docker.io" } else { &self.registry }
    }

    pub fn reference(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }

    pub fn with_digest(&self, digest: &str) -> Reference {
        Reference { digest: Some(digest.to_string()), ..self.clone() }
    }

    pub fn insecure(&self) -> bool {
        let host = self.registry.split(':').next().unwrap_or("");
        host == "localhost" || host == "127.0.0.1" || host.ends_with(".localhost") || host == "[::1]"
    }
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
        let r = Reference::parse("gcr.io/distroless/cc-debian12@sha256:abc").unwrap();
        assert_eq!(r.registry, "gcr.io");
        assert_eq!(r.tag, None);
        assert_eq!(r.digest.as_deref(), Some("sha256:abc"));
        let r = Reference::parse("user/repo:1.0@sha256:abc").unwrap();
        assert_eq!(r.repository, "user/repo");
        assert_eq!(r.tag.as_deref(), Some("1.0"));
    }
}
