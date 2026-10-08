#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    User,
    Infra,
    Config,
    Internal,
}

impl ErrorClass {
    pub fn name(self) -> &'static str {
        match self {
            ErrorClass::User => "user",
            ErrorClass::Infra => "infra",
            ErrorClass::Config => "config",
            ErrorClass::Internal => "internal",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            ErrorClass::User => 1,
            ErrorClass::Internal => 70,
            ErrorClass::Infra => 75,
            ErrorClass::Config => 78,
        }
    }
}

const KILLED: &[&str] = &["signal: 9", "exit status: 137", "out of memory", "oom"];

const INFRA: &[&str] = &[
    "is unreachable",
    "rate limited",
    "429 too many requests",
    "500 internal server error",
    "502 bad gateway",
    "503 service unavailable",
    "504 gateway timeout",
    "error sending request",
    "connection refused",
    "connection reset",
    "operation timed out",
    "timed out waiting for response",
    "stalled at",
    "upload side closed",
    "no space left on device",
    "dns error",
    "failed to lookup address",
];

const CONFIG: &[&str] = &[
    "could not detect how to build",
    "no start command found",
    "not supported yet",
    "is not valid json",
    "does not match the config schema",
    "out of sync with package.json",
    "requires credentials",
    "manifest_unknown",
    "has no manifest for",
    "unknown package manager",
    "invalid env",
    "not found",
];

pub fn classify(err: &anyhow::Error) -> ErrorClass {
    let msg = format!("{err:#}").to_ascii_lowercase();
    let has = |list: &[&str]| list.iter().any(|m| msg.contains(m));
    if has(KILLED) {
        return ErrorClass::Infra;
    }
    if msg.contains("failed with exit status") || msg.contains("script of ") {
        return ErrorClass::User;
    }
    if msg.contains("timed out after") && msg.contains("acropolis_") {
        return ErrorClass::User;
    }
    if has(INFRA) {
        return ErrorClass::Infra;
    }
    if has(CONFIG) {
        return ErrorClass::Config;
    }
    ErrorClass::Internal
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn classes() {
        assert_eq!(classify(&anyhow!("step build (run next build): `/bin/sh -c next build` failed with exit status: 1")), ErrorClass::User);
        assert_eq!(classify(&anyhow!("step build: `/bin/sh -c cargo build` failed with exit status: 137")), ErrorClass::Infra);
        assert_eq!(classify(&anyhow!("registry-1.docker.io is unreachable")), ErrorClass::Infra);
        assert_eq!(classify(&anyhow!("could not detect how to build /app: no package.json or go.mod")), ErrorClass::Config);
        assert_eq!(classify(&anyhow!("index out of bounds")), ErrorClass::Internal);
    }
}
