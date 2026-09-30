//! `list [DIR]`: a dev folder as a tree down to its repositories.

use std::path::Path;

use crate::commands::session;
use crate::listing;
use crate::report;
use crate::workspace;


// ============
// === list ===
// ============

/// The tree of `dir`, or of the workspace when no folder is given. Needs no workspace when `dir` is given.
pub(crate) fn list(context: &session::Context, dir: Option<&Path>) -> anyhow::Result<String> {
    let root = match dir {
        Some(dir) => session::resolve_path(dir)?,
        None => workspace::Workspace::discover(&context.git, context.root.as_deref())?.root().to_path_buf(),
    };
    anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
    let tree = listing::entry(&root, root.display().to_string());
    Ok(listing::render(&tree, report::use_color()))
}
