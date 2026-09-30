//! `init <DIR>` (§9.3).

use std::path::Path;

use crate::commands::session;
use crate::report;
use crate::workspace;


// ============
// === init ===
// ============

pub(crate) fn init(context: &session::Context, dir: &Path, report: &mut report::Report) -> anyhow::Result<()> {
    let root = workspace::init(&context.git, dir)?;
    let repository = workspace::Repository::of(&root);
    let (root_shown, repository_shown) = (root.display(), repository.dir().display());
    report.done(
        report::Scope::Workspace,
        format!("created the dev_sync workspace in {root_shown}; its repository is {repository_shown}"),
    );
    report.info(
        report::Scope::Workspace,
        format!(
            "next: `git -C {} remote add origin <workspace repo url>`, then `dev_sync push` from inside {root_shown}",
            repository.shell_word()
        ),
    );
    report.info(report::Scope::Workspace, "clones already inside it are recorded by the first push".to_owned());
    Ok(())
}
