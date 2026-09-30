//! What a command did and what needs the user, printed at the end (§9.10).

use std::io::IsTerminal as _;
use std::process::ExitCode;

use crate::domain;


// ================
// === Severity ===
// ================

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Severity {
    Done,
    Info,
    /// Needs the user: the command finished, but something is left for them to resolve.
    Attention,
    Failure,
}

impl Severity {
    fn marker(self) -> &'static str {
        match self {
            Self::Done => "✓",
            Self::Info => "·",
            Self::Attention => "!",
            Self::Failure => "✗",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Self::Done => "32",
            Self::Info => "2",
            Self::Attention => "33",
            Self::Failure => "31",
        }
    }
}


// =============
// === Scope ===
// =============

/// What an item is about. The declaration order is the order of the printed groups; repos sort by path.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Scope {
    Workspace,
    Layout,
    Disk,
    Repo(domain::RepoPath),
}


// ============
// === Item ===
// ============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Item {
    pub(crate) severity: Severity,
    pub(crate) scope: Scope,
    pub(crate) message: String,
}


// ==============
// === Report ===
// ==============

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub(crate) struct Report {
    items: Vec<Item>,
}

impl Report {
    pub(crate) fn push(&mut self, severity: Severity, scope: Scope, message: String) {
        self.items.push(Item { severity, scope, message });
    }

    pub(crate) fn done(&mut self, scope: Scope, message: String) {
        self.push(Severity::Done, scope, message);
    }

    pub(crate) fn info(&mut self, scope: Scope, message: String) {
        self.push(Severity::Info, scope, message);
    }

    pub(crate) fn attention(&mut self, scope: Scope, message: String) {
        self.push(Severity::Attention, scope, message);
    }

    pub(crate) fn failure(&mut self, scope: Scope, message: String) {
        self.push(Severity::Failure, scope, message);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.items.extend(other.items);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn has(&self, severity: Severity) -> bool {
        self.items.iter().any(|item| item.severity == severity)
    }

    /// One line per item, grouped by scope, each group in the order its items were added.
    pub(crate) fn render(&self, colored: bool) -> String {
        let mut items = self.items.iter().collect::<Vec<_>>();
        items.sort_by(|left, right| left.scope.cmp(&right.scope));
        items
            .into_iter()
            .map(|item| {
                let marker = match colored {
                    true => format!("\u{1b}[{}m{}\u{1b}[0m", item.severity.color(), item.severity.marker()),
                    false => item.severity.marker().to_owned(),
                };
                let message = item.message.replace('\n', "; ");
                match &item.scope {
                    Scope::Repo(path) => format!("{marker} {path}: {message}\n"),
                    Scope::Workspace | Scope::Layout | Scope::Disk => format!("{marker} {message}\n"),
                }
            })
            .collect()
    }

    /// 1 if anything failed, else 2 if anything needs the user, else 0.
    pub(crate) fn status(&self) -> u8 {
        match (self.has(Severity::Failure), self.has(Severity::Attention)) {
            (true, _) => 1,
            (false, true) => 2,
            (false, false) => 0,
        }
    }

    pub(crate) fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.status())
    }
}


// =================
// === use_color ===
// =================

/// Colors only for a terminal, and never when `NO_COLOR` is set.
pub(crate) fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}


// ==============
// === plural ===
// ==============

/// `1 commit`, `3 commits`.
pub(crate) fn plural(count: u32, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        _ => format!("{count} {noun}s"),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use super::Report;
    use super::Scope;
    use super::Severity;
    use super::plural;

    #[test]
    fn exit_status_follows_the_worst_severity() {
        let with = |severities: &[Severity]| {
            let mut report = Report::default();
            for severity in severities {
                report.push(*severity, Scope::Layout, "x".to_owned());
            }
            report.status()
        };
        assert_eq!(with(&[]), 0);
        assert_eq!(with(&[Severity::Done, Severity::Info]), 0);
        assert_eq!(with(&[Severity::Done, Severity::Attention]), 2);
        assert_eq!(with(&[Severity::Attention, Severity::Failure, Severity::Info]), 1);
        assert_eq!(with(&[Severity::Failure]), 1);
    }

    #[test]
    fn renders_one_line_per_item_grouped_by_scope() -> anyhow::Result<()> {
        let mut report = Report::default();
        report.push(Severity::Failure, Scope::Repo(fixtures::path("website")?), "fetch failed".to_owned());
        report.push(Severity::Done, Scope::Disk, "cloned ferrisoft/website".to_owned());
        report.push(
            Severity::Attention,
            Scope::Repo(fixtures::path("devman")?),
            "main diverged\nsecond line".to_owned(),
        );
        report.push(Severity::Info, Scope::Layout, "hetzner has no origin remote".to_owned());
        report.push(Severity::Done, Scope::Workspace, "pushed the layout".to_owned());
        assert_eq!(
            report.render(false),
            "✓ pushed the layout\n\
             · hetzner has no origin remote\n\
             ✓ cloned ferrisoft/website\n\
             ! devman: main diverged; second line\n\
             ✗ website: fetch failed\n"
        );
        assert!(report.render(true).contains("\u{1b}[31m✗\u{1b}[0m website: fetch failed"));
        Ok(())
    }

    #[test]
    fn counts_with_the_right_plural() {
        assert_eq!(plural(1, "commit"), "1 commit");
        assert_eq!(plural(3, "commit"), "3 commits");
        assert_eq!(plural(0, "repo"), "0 repos");
    }
}
