//! Quoting for sh.


// ===============
// === Quoting ===
// ===============

/// `text` in single quotes, each `'` written as `'\''`. Safe for any text sh can hold.
pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// `text` as is when sh would read it unchanged, otherwise quoted. For commands shown to the user.
pub(crate) fn word(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '%' | '+' | ',');
    let plain = !text.is_empty() && text.chars().all(safe);
    match plain {
        true => text.to_owned(),
        false => quote(text),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::quote;
    use super::word;

    #[test]
    fn quotes_in_single_quotes() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("/tmp/a b/$HOME `x` \"y\""), "'/tmp/a b/$HOME `x` \"y\"'");
    }

    #[test]
    fn leaves_safe_words_alone() {
        assert_eq!(word("ferrisoft/design_system"), "ferrisoft/design_system");
        assert_eq!(word("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(word("my repo"), "'my repo'");
        assert_eq!(word("zażółć"), "'zażółć'");
        assert_eq!(word(""), "''");
    }

    #[test]
    fn quoted_text_survives_sh() -> anyhow::Result<()> {
        let text = "it's a \"world\" with $dollars, `ticks` and \\ backslashes";
        let output = std::process::Command::new("sh").arg("-c").arg(format!("printf %s {}", quote(text))).output()?;
        assert_eq!(String::from_utf8(output.stdout)?, text);
        Ok(())
    }
}
