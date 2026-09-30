use std::fmt;
use std::str::FromStr;

use anyhow::Context as _;


// =================
// === RemoteUrl ===
// =================

/// A git remote URL exactly as configured: `git@github.com:o/r.git`, `https://…` or a local path.
///
/// Never empty, never padded with whitespace, free of control characters, and never starting with `-`, so it can't be
/// mistaken for a command-line option.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RemoteUrl {
    url: String,
}

impl RemoteUrl {
    pub(crate) fn as_str(&self) -> &str {
        &self.url
    }
}

impl FromStr for RemoteUrl {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        validate(text).map(|()| Self { url: text.to_owned() }).with_context(|| format!("invalid remote URL {text:?}"))
    }
}

fn validate(text: &str) -> anyhow::Result<()> {
    match text {
        "" => Err(anyhow::anyhow!("it is empty")),
        _ if text.trim() != text => Err(anyhow::anyhow!("it starts or ends with whitespace")),
        _ if text.chars().any(char::is_control) => Err(anyhow::anyhow!("it contains a control character")),
        _ if text.starts_with('-') => Err(anyhow::anyhow!("it starts with `-`")),
        _ => Ok(()),
    }
}

impl fmt::Display for RemoteUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.url)
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::RemoteUrl;

    #[test]
    fn accepts_valid_urls() -> anyhow::Result<()> {
        for text in ["git@github.com:o/r.git", "https://github.com/o/r.git", "/tmp/x/r.git", "/tmp/it's a dir/r.git"] {
            assert_eq!(text.parse::<RemoteUrl>()?.as_str(), text);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_urls() {
        for text in ["", " x", "x ", "-oProxyCommand=x", "a\nb", "a\tb"] {
            assert!(text.parse::<RemoteUrl>().is_err(), "{text:?} should be rejected");
        }
    }
}
