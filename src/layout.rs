//! The workspace layout: which repository lives at which path, how layouts change, merge, and are stored.

mod change;
mod file;
mod merge;
mod model;

pub(crate) use change::Change;
pub(crate) use change::diff;
pub(crate) use file::parse;
pub(crate) use file::parse_merge_input;
pub(crate) use file::render;
pub(crate) use file::render_conflicted;
pub(crate) use file::render_entry;
pub(crate) use merge::Conflict;
pub(crate) use merge::MergeOutcome;
pub(crate) use merge::merge;
pub(crate) use merge::stray_changes;
pub(crate) use model::Applied;
pub(crate) use model::ChangeRejection;
pub(crate) use model::Layout;
pub(crate) use model::LayoutRepo;
pub(crate) use model::RejectionReason;
