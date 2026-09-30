use std::fmt;
use std::str::FromStr;


// ==================
// === BranchName ===
// ==================

/// A local branch name such as `main` or `feature/x`, without the `refs/heads/` prefix.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BranchName {
    name: String,
}

impl BranchName {
    pub(crate) fn as_str(&self) -> &str {
        &self.name
    }
}

impl FromStr for BranchName {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let valid = !text.is_empty() && !text.chars().any(|c| c.is_whitespace() || c.is_control());
        valid.then(|| Self { name: text.to_owned() }).ok_or_else(|| anyhow::anyhow!("invalid branch name {text:?}"))
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}


// ==================
// === RemoteName ===
// ==================

/// A remote's name, such as `origin`, that is safe to pass to git as an argument: never empty, `.` (which names this
/// repository), or anything starting with `-`, which git would take for an option.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RemoteName {
    name: String,
}

impl RemoteName {
    pub(crate) fn origin() -> Self {
        Self { name: "origin".to_owned() }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.name
    }
}

impl FromStr for RemoteName {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let valid = !text.is_empty()
            && text != "."
            && !text.starts_with('-')
            && !text.chars().any(|c| c.is_whitespace() || c.is_control());
        valid.then(|| Self { name: text.to_owned() }).ok_or_else(|| anyhow::anyhow!("invalid remote name {text:?}"))
    }
}

impl fmt::Display for RemoteName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}


// ================
// === CommitId ===
// ================

/// A full commit hash: 40 (SHA-1) or 64 (SHA-256) hex digits.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CommitId {
    hash: String,
}

impl FromStr for CommitId {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        let valid = matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit());
        valid.then(|| Self { hash: text.to_owned() }).ok_or_else(|| anyhow::anyhow!("invalid commit id {text:?}"))
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hash)
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::BranchName;
    use super::CommitId;
    use super::RemoteName;

    #[test]
    fn validates_branch_names() {
        for text in ["main", "feature/x", "wojtek/fix-1"] {
            assert!(text.parse::<BranchName>().is_ok(), "{text:?} should be accepted");
        }
        for text in ["", "a b", "a\tb", "a\u{7f}"] {
            assert!(text.parse::<BranchName>().is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn validates_remote_names() {
        for text in ["origin", "upstream", "my-fork", "a.b"] {
            assert!(text.parse::<RemoteName>().is_ok(), "{text:?} should be accepted");
        }
        for text in ["", ".", "-x", "--mirror", "a b", "a\nb"] {
            assert!(text.parse::<RemoteName>().is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn validates_commit_ids() {
        let sha1 = "a3c958aec2859e10e9ab44477a1b0740ec3f753c";
        let sha256 = "a3c958aec2859e10e9ab44477a1b0740ec3f753ca3c958aec2859e10e9ab4447";
        assert!(sha1.parse::<CommitId>().is_ok());
        assert!(sha256.parse::<CommitId>().is_ok());
        for text in ["", "a3c958a", "g3c958aec2859e10e9ab44477a1b0740ec3f753c", "(initial)"] {
            assert!(text.parse::<CommitId>().is_err(), "{text:?} should be rejected");
        }
    }
}
