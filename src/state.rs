//! The per-machine base (§6.2) and the command lock (§6.3).

use std::collections::BTreeMap;
use std::fs::File;
use std::fs::TryLockError;
use std::io::ErrorKind;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain;


// ===================
// === KnownStatus ===
// ===================

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KnownStatus {
    Synced,
    /// Removed from the layout elsewhere, but kept on disk because it holds work that exists only here.
    RemovalBlocked,
}


// =================
// === KnownRepo ===
// =================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KnownRepo {
    pub(crate) url: domain::RemoteUrl,
    pub(crate) id: domain::FileId,
    pub(crate) status: KnownStatus,
}


// ====================
// === MachineState ===
// ====================

const FORMAT: i64 = 1;

/// What this machine had on disk after its last successful sync. Never committed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MachineState {
    pub(crate) repos: BTreeMap<domain::RepoPath, KnownRepo>,
}

impl MachineState {
    pub(crate) fn blocked(&self) -> impl Iterator<Item = &domain::RepoPath> {
        self.repos.iter().filter(|(_, known)| known.status == KnownStatus::RemovalBlocked).map(|(path, _)| path)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    format: i64,
    #[serde(default)]
    repos: BTreeMap<String, RawKnown>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct RawKnown {
    url: String,
    device: u64,
    inode: u64,
    status: RawStatus,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum RawStatus {
    Synced,
    RemovalBlocked,
}

/// A missing file is an empty base: the first run on this machine.
pub(crate) fn load(path: &Path) -> anyhow::Result<MachineState> {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(MachineState::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        Ok(text) => parse(&text).with_context(|| {
            format!("{} is not a valid dev_sync state file (deleting it is harmless)", path.display())
        }),
    }
}

fn parse(text: &str) -> anyhow::Result<MachineState> {
    let raw = toml::from_str::<RawState>(text)?;
    anyhow::ensure!(
        raw.format == FORMAT,
        "it has format {}, but this dev_sync understands format {FORMAT}",
        raw.format
    );
    let repos = raw
        .repos
        .into_iter()
        .map(|(key, known)| {
            let path = key.parse::<domain::RepoPath>()?;
            let status = match known.status {
                RawStatus::Synced => KnownStatus::Synced,
                RawStatus::RemovalBlocked => KnownStatus::RemovalBlocked,
            };
            let id = domain::FileId { device: known.device, inode: known.inode };
            Ok((path, KnownRepo { url: known.url.parse()?, id, status }))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    Ok(MachineState { repos })
}

/// Replaces the file atomically: a crash leaves either the old or the new state, never a torn one.
pub(crate) fn save(path: &Path, state: &MachineState) -> anyhow::Result<()> {
    let dir = path.parent().with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let repos = state.repos.iter().map(|(path, known)| {
        let status = match known.status {
            KnownStatus::Synced => RawStatus::Synced,
            KnownStatus::RemovalBlocked => RawStatus::RemovalBlocked,
        };
        let raw = RawKnown { url: known.url.to_string(), device: known.id.device, inode: known.id.inode, status };
        (path.to_string(), raw)
    });
    let text = toml::to_string(&RawState { format: FORMAT, repos: repos.collect() })
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    let staged = sibling(path, ".tmp")?;
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, path)?;
        File::open(dir)?.sync_all()
    };
    write().with_context(|| format!("failed to write {}", path.display()))
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let name = path.file_name().with_context(|| format!("{} has no file name", path.display()))?;
    let mut name = name.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}


// ============
// === Lock ===
// ============

/// Held while a command runs: an exclusive `flock` on the lock file, which the kernel releases when the process ends,
/// however it ends — so a crash never leaves a stale lock, and there is no process id to check or to reuse. The file
/// stays; it holds the process id of the last holder, for the message.
#[derive(Debug)]
#[must_use]
pub(crate) struct Lock {
    _file: File,
}

impl Lock {
    pub(crate) fn acquire(path: &Path) -> anyhow::Result<Self> {
        let dir = path.parent().with_context(|| format!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open the lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {
                let pid = std::process::id().to_string();
                file.set_len(0)
                    .and_then(|()| (&file).write_all(pid.as_bytes()))
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(Self { _file: file })
            }
            Err(TryLockError::WouldBlock) => {
                let holder = std::fs::read_to_string(path).ok().and_then(|text| text.trim().parse::<u32>().ok());
                Err(match holder {
                    Some(pid) => anyhow::anyhow!("another dev_sync is running (pid {pid})"),
                    None => anyhow::anyhow!("another dev_sync is running"),
                })
            }
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("failed to lock {}", path.display()))
            }
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use crate::domain;
    use crate::fixtures;
    use super::KnownRepo;
    use super::KnownStatus;
    use super::Lock;
    use super::MachineState;
    use super::load;
    use super::save;

    fn sample() -> anyhow::Result<MachineState> {
        let known = |url: &str, inode: u64, status| -> anyhow::Result<KnownRepo> {
            Ok(KnownRepo { url: fixtures::url(url)?, id: domain::FileId { device: 43, inode }, status })
        };
        Ok(MachineState {
            repos: BTreeMap::from([
                (
                    fixtures::path("account_manager")?,
                    known("git@github.com:ferrisoft/am.git", 1234567, KnownStatus::Synced)?,
                ),
                (
                    fixtures::path("zażółć/my \"repo\"")?,
                    known("/tmp/it's here/r.git", 7, KnownStatus::RemovalBlocked)?,
                ),
            ]),
        })
    }

    #[test]
    fn round_trips_through_the_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("dev_sync").join("state.toml");
        let state = sample()?;
        save(&path, &state)?;
        assert_eq!(load(&path)?, state);
        let text = std::fs::read_to_string(&path)?;
        assert!(text.starts_with("format = 1\n"), "{text}");
        assert!(text.contains("status = \"removal-blocked\""), "{text}");
        Ok(())
    }

    #[test]
    fn a_missing_file_is_an_empty_base() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(load(&dir.path().join("state.toml"))?, MachineState::default());
        Ok(())
    }

    #[test]
    fn a_bad_file_is_an_error_naming_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.toml");
        let texts = [
            "format = 2\n",
            "format = 1\n[repos.a]\nurl = \"u\"\n",
            "not toml",
            "format = 1\n[repos.\"../x\"]\nurl = \"u\"\ndevice = 1\ninode = 2\nstatus = \"synced\"\n",
        ];
        for text in texts {
            std::fs::write(&path, text)?;
            let message = load(&path).err().map(|error| format!("{error:#}")).unwrap_or_default();
            assert!(message.contains("state.toml"), "{text:?}: {message}");
        }
        Ok(())
    }

    #[test]
    fn save_replaces_the_file_atomically() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.toml");
        save(&path, &sample()?)?;
        save(&path, &MachineState::default())?;
        assert_eq!(load(&path)?, MachineState::default());
        let names = std::fs::read_dir(dir.path())?
            .map(|entry| Ok(entry?.file_name()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(names, vec![std::ffi::OsString::from("state.toml")]);
        Ok(())
    }

    /// Takes the lock, waiting a moment if needed: a child that another test thread is starting shares the lock's
    /// file description until it runs its program, so a lock can stay taken briefly after its holder is dropped.
    fn acquire_soon(path: &Path) -> anyhow::Result<Lock> {
        let mut waited = 0_u32;
        loop {
            match Lock::acquire(path) {
                Ok(lock) => break Ok(lock),
                Err(_) if waited < 200 => {
                    waited = waited.saturating_add(1);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => break Err(error),
            }
        }
    }

    #[test]
    fn a_lock_is_taken_and_released_on_drop() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("dev_sync").join("lock");
        let lock = Lock::acquire(&path)?;
        assert_eq!(std::fs::read_to_string(&path)?, std::process::id().to_string());
        drop(lock);
        drop(acquire_soon(&path)?);
        Ok(())
    }

    #[test]
    fn a_held_lock_refuses_everyone_else() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lock");
        let held = Lock::acquire(&path)?;
        let message = Lock::acquire(&path).err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains(&format!("another dev_sync is running (pid {})", std::process::id())), "{message}");
        drop(held);
        Ok(())
    }

    #[test]
    fn a_holder_that_died_leaves_no_stale_lock() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lock");
        let mut child = std::process::Command::new("true").spawn()?;
        let dead = child.id();
        child.wait()?;
        for leftover in [dead.to_string(), std::process::id().to_string(), "garbage".to_owned()] {
            std::fs::write(&path, leftover)?;
            drop(acquire_soon(&path)?);
        }
        Ok(())
    }
}
