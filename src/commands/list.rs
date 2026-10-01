//! `list [DIR]`: a dev folder as a tree down to its repositories, with notes on each repository.

use std::collections::BTreeMap;
use std::path::Path;

use crate::commands::session;
use crate::git;
use crate::listing;
use crate::parallel;
use crate::report;
use crate::workspace;


// ============
// === list ===
// ============

/// The tree of `dir`, or of the workspace when no folder is given. Needs no workspace when `dir` is given, and no
/// network: the notes are as of each repository's last fetch.
pub(crate) fn list(context: &session::Context, dir: Option<&Path>) -> anyhow::Result<String> {
    let git = &context.git;
    let root = match dir {
        Some(dir) => session::resolve_path(dir)?,
        None => workspace::Workspace::discover(git, context.root.as_deref())?.root().to_path_buf(),
    };
    anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
    let tree = listing::entry(&root, root.display().to_string());
    let repositories = tree.repositories();
    let described = parallel::map(&repositories, git.policy().parallelism, |path| describe(git, path))?;
    let mut notes = repositories.into_iter().zip(described).collect::<BTreeMap<_, _>>();
    Ok(listing::render(&tree.with_notes(&mut notes), report::use_color()))
}

/// The notes on the repository at `path`; one git can't read says so instead.
fn describe(git: &git::Git, path: &Path) -> Vec<listing::Note> {
    let described = git::inspect(git, path).and_then(|status| Ok(listing::notes(&status, &git::origin(git, path)?)));
    described.unwrap_or_else(|error| {
        vec![listing::Note { text: format!("can't read it: {error}"), tone: listing::Tone::Problem }]
    })
}
