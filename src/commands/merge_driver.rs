//! `merge-driver <BASE> <LOCAL> <INCOMING> <PATH>`: the git merge driver for `repos.toml` (§9.9).

use std::path::Path;

use anyhow::Context as _;

use crate::git;
use crate::layout;


// =====================
// === DriverOutcome ===
// =====================

/// What the driver tells git. An error is neither: git must see it as a failed merge, not as a conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum DriverOutcome {
    Clean,
    Conflicted,
}

impl DriverOutcome {
    /// Git reads 0 as merged, 1 to 128 as a conflict, and anything higher as a failed driver.
    pub(crate) fn exit_code(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Conflicted => 1,
        }
    }
}

/// The exit code for a driver that failed, so git aborts the merge instead of recording a conflict.
pub(crate) const DRIVER_FAILED: u8 = 255;


// ====================
// === merge_driver ===
// ====================

/// Merges the three versions git hands over (temporary files relative to the current directory, which is why the
/// directory never changes) and writes the result into `local`. When any input doesn't parse, falls back to git's
/// own text merge.
pub(crate) fn merge_driver(
    git: &git::Git,
    base: &Path,
    local: &Path,
    incoming: &Path,
) -> anyhow::Result<DriverOutcome> {
    let read = |path: &Path| std::fs::read(path).with_context(|| format!("failed to read {}", path.display()));
    let texts = [read(base)?, read(local)?, read(incoming)?];
    let parsed = texts.map(|bytes| {
        String::from_utf8(bytes).map_err(anyhow::Error::from).and_then(|text| layout::parse_merge_input(&text))
    });
    match parsed {
        [Ok(base_layout), Ok(local_layout), Ok(incoming_layout)] => {
            let merged = layout::merge(&base_layout, &local_layout, &incoming_layout);
            let text = match &merged {
                layout::MergeOutcome::Clean(merged) => layout::render(merged),
                layout::MergeOutcome::Conflicted(conflicted) => layout::render_conflicted(conflicted),
            };
            std::fs::write(local, text).with_context(|| format!("failed to write {}", local.display()))?;
            Ok(match merged {
                layout::MergeOutcome::Clean(_) => DriverOutcome::Clean,
                layout::MergeOutcome::Conflicted(_) => DriverOutcome::Conflicted,
            })
        }
        unparsable => {
            let problems = unparsable.into_iter().filter_map(Result::err).map(|error| format!("{error:#}"));
            tracing::warn!(problems = ?problems.collect::<Vec<_>>(), "merging repos.toml as text");
            text_merge(git, base, local, incoming)
        }
    }
}

fn text_merge(git: &git::Git, base: &Path, local: &Path, incoming: &Path) -> anyhow::Result<DriverOutcome> {
    let finished = git
        .outside()
        .args(["merge-file", "-L", "local", "-L", "base", "-L", "incoming", "--"])
        .args([local, base, incoming])
        .run(git::Access::Write)?;
    match finished.code {
        Some(0) => Ok(DriverOutcome::Clean),
        Some(1..=127) => Ok(DriverOutcome::Conflicted),
        _ => Err(anyhow::anyhow!("git merge-file failed ({}): {}", finished.exit(), finished.error_text())),
    }
}
