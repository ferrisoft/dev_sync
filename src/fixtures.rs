//! Shorthand constructors and sandboxes for unit tests.

use std::ffi::OsString;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;

use crate::domain;
use crate::git;
use crate::layout;
use crate::workspace;


// ==============
// === Values ===
// ==============

pub(crate) fn path(text: &str) -> anyhow::Result<domain::RepoPath> {
    text.parse()
}

pub(crate) fn url(text: &str) -> anyhow::Result<domain::RemoteUrl> {
    text.parse()
}

/// A layout from `path=url` pairs separated by spaces, e.g. `"a=u1 x/b=u2"`.
pub(crate) fn layout(spec: &str) -> anyhow::Result<layout::Layout> {
    let repos = spec
        .split_whitespace()
        .map(|pair| {
            let (path_text, url_text) =
                pair.split_once('=').ok_or_else(|| anyhow::anyhow!("fixture pair {pair:?} has no `=`"))?;
            Ok(layout::LayoutRepo { path: path(path_text)?, url: url(url_text)? })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    layout::Layout::from_repos(repos)
}


// ===============
// === Changes ===
// ===============

pub(crate) fn add(at: &str, to: &str) -> anyhow::Result<layout::Change> {
    Ok(layout::Change::Add { path: path(at)?, url: url(to)? })
}

pub(crate) fn remove(at: &str, of: &str) -> anyhow::Result<layout::Change> {
    Ok(layout::Change::Remove { path: path(at)?, url: url(of)? })
}

pub(crate) fn move_to(from: &str, to: &str, of: &str) -> anyhow::Result<layout::Change> {
    Ok(layout::Change::Move { from: path(from)?, to: path(to)?, url: url(of)? })
}

pub(crate) fn set_url(at: &str, from: &str, to: &str) -> anyhow::Result<layout::Change> {
    Ok(layout::Change::SetUrl { path: path(at)?, from: url(from)?, to: url(to)? })
}


// ===========
// === Git ===
// ===========

/// A runner with short time limits and retry delays, isolated from the developer's git configuration (§13.3).
pub(crate) fn git() -> git::Git {
    git_with_policy(git::NetworkPolicy {
        stall_limit: Duration::from_secs(20),
        local_timeout: Duration::from_secs(20),
        attempts: 3,
        retry_base: Duration::from_millis(10),
        parallelism: 8,
    })
}

/// A runner that stops a network or lengthy command once it has printed nothing for `limit`.
pub(crate) fn git_with_stall_limit(limit: Duration) -> git::Git {
    git_with_policy(git::NetworkPolicy { stall_limit: limit, ..git().policy().clone() })
}

/// A runner as if the user's configuration also set `key` to `value`.
pub(crate) fn git_configured(key: &str, value: &str) -> git::Git {
    git_with_config(git().policy().clone(), &[Setting { key, value }])
}

fn git_with_policy(policy: git::NetworkPolicy) -> git::Git {
    git_with_config(policy, &[])
}

/// One git configuration setting.
struct Setting<'a> {
    key: &'a str,
    value: &'a str,
}

/// No system or global configuration, not even the default `~/.config/git/ignore`; a fixed identity and branch name,
/// and `extra` settings.
fn git_with_config(policy: git::NetworkPolicy, extra: &[Setting<'_>]) -> git::Git {
    let identity = [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_AUTHOR_NAME", "Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
    ];
    let defaults = [
        Setting { key: "init.defaultBranch", value: "main" },
        Setting { key: "core.excludesFile", value: "/dev/null" },
    ];
    let config = defaults.iter().chain(extra).zip(0_usize..).flat_map(|(setting, index)| {
        [
            (format!("GIT_CONFIG_KEY_{index}"), setting.key.to_owned()),
            (format!("GIT_CONFIG_VALUE_{index}"), setting.value.to_owned()),
        ]
    });
    let count = extra.len().saturating_add(2).to_string();
    let environment = identity
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .chain([("GIT_CONFIG_COUNT".to_owned(), count)])
        .chain(config)
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
    git::Git::with_environment(policy, environment.collect())
}


// ===============
// === Sandbox ===
// ===============

/// A temporary directory with its own git configuration, for creating repositories in tests.
pub(crate) struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub(crate) fn create() -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("home"))?;
        std::fs::write(
            dir.path().join("gitconfig"),
            "[user]\n\tname = Test\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n[core]\n\texcludesFile = /dev/null\n",
        )?;
        Ok(Self { dir })
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs git in `dir` with the sandbox configuration; it must succeed. Returns stdout.
    pub(crate) fn git(&self, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", self.path().join("home"))
            .env("GIT_CONFIG_GLOBAL", self.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .context("failed to run git")?;
        match output.status.success() {
            true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            false => Err(anyhow::anyhow!(
                "git {args:?} in {} failed: {}",
                dir.display(),
                String::from_utf8_lossy(&output.stderr)
            )),
        }
    }

    /// A bare repository with one commit on `main`.
    pub(crate) fn remote(&self, name: &str) -> anyhow::Result<PathBuf> {
        let bare = self.path().join("remotes").join(format!("{name}.git"));
        std::fs::create_dir_all(&bare)?;
        self.git(&bare, &["init", "--quiet", "--bare"])?;
        let seed = self.path().join("seeds").join(name);
        std::fs::create_dir_all(&seed)?;
        self.git(&seed, &["init", "--quiet"])?;
        std::fs::write(seed.join("README"), format!("{name}\n"))?;
        self.git(&seed, &["add", "README"])?;
        self.git(&seed, &["commit", "--quiet", "-m", "first"])?;
        self.git(&seed, &["push", "--quiet", &bare.to_string_lossy(), "main"])?;
        Ok(bare)
    }

    /// A bare copy of `remote` with the same history, as after moving a repository to a new URL.
    pub(crate) fn mirror(&self, remote: &Path, name: &str) -> anyhow::Result<PathBuf> {
        let bare = self.path().join("remotes").join(format!("{name}.git"));
        self.git(self.path(), &["clone", "--quiet", "--bare", &remote.to_string_lossy(), &bare.to_string_lossy()])?;
        Ok(bare)
    }

    /// A clone of `remote` at `dir`.
    pub(crate) fn clone(&self, remote: &Path, dir: &Path) -> anyhow::Result<PathBuf> {
        let parent = dir.parent().context("clone target has no parent")?;
        std::fs::create_dir_all(parent)?;
        self.git(parent, &["clone", "--quiet", &remote.to_string_lossy(), &dir.to_string_lossy()])?;
        Ok(dir.to_path_buf())
    }

    /// Writes `file` and commits it.
    pub(crate) fn commit(&self, repo: &Path, file: &str, content: &str) -> anyhow::Result<()> {
        std::fs::write(repo.join(file), content)?;
        self.git(repo, &["add", file])?;
        self.git(repo, &["commit", "--quiet", "-m", &format!("change {file}")])?;
        Ok(())
    }
}


// =================
// === workspace ===
// =================

/// A workspace rooted at `root`, as `init` makes one before publishing it: the repository in `.dev_sync`, populated
/// and committed, without a remote. Returns the canonical root.
pub(crate) fn workspace(root: &Path) -> anyhow::Result<PathBuf> {
    let (git, repository) = (git(), workspace::Repository::of(root));
    std::fs::create_dir_all(repository.dir())?;
    let initialized = git.outside().args(["init", "--quiet", "--initial-branch=main", "--"]).arg(repository.dir());
    initialized.run_ok(git::Access::Write)?;
    workspace::populate(&git, &repository, &url("https://example.invalid/dev.git")?)?;
    Ok(root.canonicalize()?)
}


// ===================
// === closed_port ===
// ===================

/// A local port nothing listens on.
pub(crate) fn closed_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}


// ==============
// === Server ===
// ==============

/// A TCP server on 127.0.0.1 that runs `respond` on every connection until dropped.
pub(crate) struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    /// Accepts connections and never answers.
    pub(crate) fn silent() -> anyhow::Result<Self> {
        Self::start(|stream, stop| {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
            drop(stream);
        })
    }

    /// Answers every HTTP request with `401 Unauthorized`.
    pub(crate) fn unauthorized() -> anyhow::Result<Self> {
        Self::start(|mut stream, _stop| {
            let mut request = Vec::<u8>::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => request.extend(buffer.iter().take(count)),
                }
            }
            let response = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"x\"\r\n\
                            Content-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(response.as_bytes()).ok();
        })
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/x.git", self.port)
    }

    fn start<F>(respond: F) -> anyhow::Result<Self> where
    F: Fn(TcpStream, &AtomicBool) + Send + Sync + 'static {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let respond = Arc::new(respond);
        let thread = thread::Builder::new().name("test server".to_owned()).spawn(move || {
            thread::scope(|scope| {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let respond = Arc::clone(&respond);
                        let flag = &flag;
                        scope.spawn(move || respond(stream, flag));
                    }
                }
            });
        })?;
        Ok(Self { port, stop, thread: Some(thread) })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        TcpStream::connect(("127.0.0.1", self.port)).ok();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}
