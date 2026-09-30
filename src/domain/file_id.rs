use std::fs::Metadata;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use anyhow::Context as _;


// ==============
// === FileId ===
// ==============

/// Identifies a file or directory across renames within one filesystem.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FileId {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl FileId {
    /// Reads the id without following a symlink at `path`.
    pub(crate) fn of(path: &Path) -> anyhow::Result<Self> {
        std::fs::symlink_metadata(path)
            .map(|metadata| Self::from_metadata(&metadata))
            .with_context(|| format!("failed to read the metadata of {}", path.display()))
    }

    pub(crate) fn from_metadata(metadata: &Metadata) -> Self {
        Self { device: metadata.dev(), inode: metadata.ino() }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::FileId;

    #[test]
    fn survives_a_rename_and_differs_between_directories() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first)?;
        std::fs::create_dir(&second)?;
        let before = FileId::of(&first)?;
        assert_ne!(before, FileId::of(&second)?);
        let moved = dir.path().join("moved");
        std::fs::rename(&first, &moved)?;
        assert_eq!(FileId::of(&moved)?, before);
        Ok(())
    }

    #[test]
    fn fails_for_a_missing_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(FileId::of(&dir.path().join("missing")).is_err());
        Ok(())
    }
}
