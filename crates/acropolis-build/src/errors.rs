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

const KILLED: &[&str] = &["out of memory", "oom-kill", "oom killer"];

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
    "get blob sha256:",
    "falling back to mirror",
    "build interrupted",
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
    "version matches",
    "not found in",
    "does not exist",
    "must stay inside the app directory",
    "must be a relative path",
    "unsafe install path",
    "resolves outside",
    ", outside of ",
    "points outside",
    "refusing to write outside",
    "over plain http",
    "private or link-local",
    "invalid toolchain version",
];

pub fn classify(err: &anyhow::Error) -> ErrorClass {
    if let Some(cf) = err
        .chain()
        .find_map(|e| e.downcast_ref::<acropolis_exec::CommandFailed>())
    {
        return if cf.killed() {
            ErrorClass::Infra
        } else {
            ErrorClass::User
        };
    }
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
        assert_eq!(
            classify(&anyhow!(
                "step build (run next build): `/bin/sh -c next build` failed with exit status: 1"
            )),
            ErrorClass::User
        );
        assert_eq!(
            classify(&anyhow!("registry-1.docker.io is unreachable")),
            ErrorClass::Infra
        );
        assert_eq!(
            classify(&anyhow!(
                "could not detect how to build /app: no package.json or go.mod"
            )),
            ErrorClass::Config
        );
        assert_eq!(classify(&anyhow!("index out of bounds")), ErrorClass::Internal);
        assert_eq!(
            classify(&anyhow!(
                "step packages: GET blob sha256:d4c4 from docker.io/library/debian@sha256:a467: 404 Not Found"
            )),
            ErrorClass::Infra
        );
        assert_eq!(classify(&anyhow!("no Node version matches \"99\"")), ErrorClass::Config);
        let failed = |code: i32, tail: &str| {
            use std::os::unix::process::ExitStatusExt;
            anyhow::Error::new(acropolis_exec::CommandFailed::new(
                &["/bin/sh".into(), "-c".into(), "vite build".into()],
                std::process::ExitStatus::from_raw(code << 8),
                vec![tail.into()],
            ))
            .context("step build (run vite build)")
        };
        assert_eq!(
            classify(&failed(
                1,
                "error: zoom.tsx: room not found, connection refused, 503 Service Unavailable"
            )),
            ErrorClass::User
        );
        assert_eq!(classify(&failed(137, "")), ErrorClass::Infra);
    }
}
