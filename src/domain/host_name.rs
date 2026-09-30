use std::fmt;
use std::str::FromStr;


// ================
// === HostName ===
// ================

/// The machine's name. Never empty, never padded, no control characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostName {
    name: String,
}

impl HostName {
    /// Tries `DEV_SYNC_HOST`, the kernel hostname, then `HOSTNAME`, and falls back to `unknown-host`.
    pub(crate) fn detect() -> Self {
        Self::first_valid([
            std::env::var("DEV_SYNC_HOST").ok(),
            std::fs::read_to_string("/proc/sys/kernel/hostname").ok(),
            std::env::var("HOSTNAME").ok(),
        ])
    }

    fn first_valid(candidates: [Option<String>; 3]) -> Self {
        candidates
            .into_iter()
            .flatten()
            .find_map(|candidate| candidate.trim().parse().ok())
            .unwrap_or_else(|| Self { name: "unknown-host".to_owned() })
    }
}

impl FromStr for HostName {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let valid = !text.is_empty() && text.trim() == text && !text.chars().any(char::is_control);
        valid.then(|| Self { name: text.to_owned() }).ok_or_else(|| anyhow::anyhow!("invalid host name {text:?}"))
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::HostName;

    #[test]
    fn prefers_the_first_usable_candidate() {
        let pick = |candidates: [Option<&str>; 3]| {
            HostName::first_valid(candidates.map(|c| c.map(str::to_owned))).to_string()
        };
        assert_eq!(pick([Some("laptop"), Some("x"), Some("y")]), "laptop");
        assert_eq!(pick([None, Some("demeter\n"), Some("y")]), "demeter");
        assert_eq!(pick([Some("  "), None, Some("dev-1")]), "dev-1");
        assert_eq!(pick([Some("a\u{1b}b"), None, None]), "unknown-host");
        assert_eq!(pick([None, None, None]), "unknown-host");
    }
}
