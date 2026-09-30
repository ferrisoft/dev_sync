//! A world of machines that share bare remotes, all inside one temporary directory. Every process gets an isolated
//! environment: its own HOME, git config, XDG directories (so the Trash is private) and a short stall limit.

use std::ffi::OsStr;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;


// =============
// === World ===
// =============

pub struct World {
    _dir: tempfile::TempDir,
    base: PathBuf,
}

impl World {
    pub fn create() -> anyhow::Result<Self> {
        Self::under("world")
    }

    /// A world whose every path contains a space, a `'` and non-ASCII.
    pub fn with_unusual_path() -> anyhow::Result<Self> {
        Self::under("it's a wörld")
    }

    pub fn path(&self) -> &Path {
        &self.base
    }

    /// A bare copy of `remote` with the same history, as after moving a repository to a new URL.
    pub fn mirror(&self, remote: &Path, name: &str) -> anyhow::Result<PathBuf> {
        let bare = self.base.join("remotes").join(format!("{name}.git"));
        self.git(&self.base, &["clone", "--quiet", "--bare", path_str(remote)?, path_str(&bare)?])?;
        Ok(bare)
    }

    pub fn machine(&self, name: &str) -> anyhow::Result<Machine<'_>> {
        let dir = self.base.join(name);
        for sub in ["home", "xdg-data", "xdg-cache"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Machine { world: self, name: name.to_owned(), dir })
    }

    /// Git with the world's configuration, outside any machine.
    pub fn git(&self, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args);
        isolate(&mut command, &self.base.join("home"), &self.base.join("gitconfig"));
        succeed(command.output()?, &format!("git {args:?}"))
    }

    /// A bare repository with one commit on `main`.
    pub fn remote(&self, name: &str) -> anyhow::Result<PathBuf> {
        let bare = self.empty_remote(name)?;
        let seed = self.base.join("seeds").join(name);
        std::fs::create_dir_all(&seed)?;
        self.git(&seed, &["init", "--quiet"])?;
        std::fs::write(seed.join("README"), format!("{name}\n"))?;
        self.git(&seed, &["add", "README"])?;
        self.git(&seed, &["commit", "--quiet", "-m", "first"])?;
        self.git(&seed, &["push", "--quiet", path_str(&bare)?, "main"])?;
        Ok(bare)
    }

    pub fn empty_remote(&self, name: &str) -> anyhow::Result<PathBuf> {
        let bare = self.base.join("remotes").join(format!("{name}.git"));
        std::fs::create_dir_all(&bare)?;
        self.git(&bare, &["init", "--quiet", "--bare"])?;
        Ok(bare)
    }

    /// Adds a commit to `remote` from a scratch clone.
    pub fn commit_to(&self, remote: &Path, file: &str, content: &str) -> anyhow::Result<()> {
        let scratch = self.base.join("scratch").join(format!("{file}-{}", content.len()));
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch)?;
        }
        std::fs::create_dir_all(&scratch)?;
        self.git(&scratch, &["clone", "--quiet", path_str(remote)?, "."])?;
        std::fs::write(scratch.join(file), content)?;
        self.git(&scratch, &["add", file])?;
        self.git(&scratch, &["commit", "--quiet", "-m", &format!("change {file}")])?;
        self.git(&scratch, &["push", "--quiet"])?;
        std::fs::remove_dir_all(&scratch)?;
        Ok(())
    }

    fn under(name: &str) -> anyhow::Result<Self> {
        let dir = tempfile::tempdir()?;
        let base = dir.path().join(name);
        std::fs::create_dir_all(base.join("remotes"))?;
        std::fs::create_dir_all(base.join("home"))?;
        std::fs::write(
            base.join("gitconfig"),
            "[user]\n\tname = Test\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n",
        )?;
        let base = base.canonicalize()?;
        Ok(Self { _dir: dir, base })
    }
}


// ===============
// === Machine ===
// ===============

pub struct Machine<'a> {
    world: &'a World,
    name: String,
    dir: PathBuf,
}

impl Machine<'_> {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn dev2(&self) -> PathBuf {
        self.dir.join("dev2")
    }

    /// The workspace repository, hidden in the dev folder.
    pub fn workspace(&self) -> PathBuf {
        self.dev2().join(".dev_sync")
    }

    pub fn trash(&self) -> PathBuf {
        self.dir.join("xdg-data").join("Trash").join("files")
    }

    /// Runs dev_sync in the machine's workspace (or its directory before there is one).
    pub fn run(&self, args: &[&str]) -> anyhow::Result<Run> {
        let cwd = match self.dev2().exists() {
            true => self.dev2(),
            false => self.dir.clone(),
        };
        self.run_in(&cwd, args, &[])
    }

    pub fn run_in(&self, cwd: &Path, args: &[&str], env: &[Variable<'_>]) -> anyhow::Result<Run> {
        let mut command = self.command(cwd, args);
        for variable in env {
            command.env(variable.name, variable.value);
        }
        Ok(Run::from(command.output()?))
    }

    /// The dev_sync command with the machine's environment, for tests that need their own stdio.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dev_sync"));
        command.current_dir(cwd).args(args);
        self.isolated(&mut command);
        command
    }

    /// Git with the machine's environment; it must succeed. Returns stdout.
    pub fn git(&self, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
        succeed(self.try_git(dir, args)?, &format!("git {args:?} in {}", dir.display()))
    }

    pub fn try_git(&self, dir: &Path, args: &[&str]) -> anyhow::Result<Output> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args);
        self.isolated(&mut command);
        Ok(command.output()?)
    }

    /// Runs `init` on the machine's dev folder with `remote` as the workspace repository: it creates the workspace when
    /// `remote` is empty and joins it otherwise.
    pub fn init_workspace(&self, remote: &Path) -> anyhow::Result<Run> {
        self.run_in(&self.dir, &["init", path_str(&self.dev2())?, "--remote", path_str(remote)?], &[])?.ok()
    }

    /// Clones `remote` into the workspace at `relative`.
    pub fn clone_into(&self, remote: &Path, relative: &str) -> anyhow::Result<PathBuf> {
        let target = self.dev2().join(relative);
        let parent = target.parent().context("no parent")?;
        std::fs::create_dir_all(parent)?;
        self.git(parent, &["clone", "--quiet", path_str(remote)?, path_str(&target)?])?;
        Ok(target)
    }

    pub fn commit(&self, repo: &Path, file: &str, content: &str) -> anyhow::Result<()> {
        std::fs::write(repo.join(file), content)?;
        self.git(repo, &["add", file])?;
        self.git(repo, &["commit", "--quiet", "-m", &format!("change {file}")])?;
        Ok(())
    }

    pub fn layout(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(self.workspace().join("repos.toml"))?)
    }

    pub fn head(&self, repo: &Path) -> anyhow::Result<String> {
        Ok(self.git(repo, &["rev-parse", "HEAD"])?.trim().to_owned())
    }

    pub fn origin(&self, repo: &Path) -> anyhow::Result<String> {
        Ok(self.git(repo, &["remote", "get-url", "origin"])?.trim().to_owned())
    }

    pub fn last_subject(&self) -> anyhow::Result<String> {
        Ok(self.git(&self.workspace(), &["log", "-1", "--format=%s"])?.trim().to_owned())
    }

    pub fn commit_count(&self) -> anyhow::Result<usize> {
        Ok(self.git(&self.workspace(), &["rev-list", "--count", "HEAD"])?.trim().parse()?)
    }

    fn isolated(&self, command: &mut Command) {
        isolate(command, &self.dir.join("home"), &self.world.base.join("gitconfig"));
        command
            .env("XDG_DATA_HOME", self.dir.join("xdg-data"))
            .env("XDG_CACHE_HOME", self.dir.join("xdg-cache"))
            .env("DEV_SYNC_HOST", &self.name)
            .env("DEV_SYNC_NETWORK_TIMEOUT_SECS", "30")
            .env("DEV_SYNC_RETRY_BASE_DELAY_MS", "10");
    }
}


// ================
// === Variable ===
// ================

/// An environment variable for one run, on top of the machine's environment.
pub struct Variable<'a> {
    pub name: &'a str,
    pub value: &'a OsStr,
}


// =================
// === Processes ===
// =================

/// Variables from the developer's environment that would change what git or dev_sync does.
const FOREIGN_VARIABLES: [&str; 21] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_SHALLOW_FILE",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "NO_COLOR",
    "DEV_SYNC_LOG",
    "DEV_SYNC_HOST",
    "DEV_SYNC_NETWORK_TIMEOUT_SECS",
];

fn isolate(command: &mut Command, home: &Path, gitconfig: &Path) {
    for variable in FOREIGN_VARIABLES {
        command.env_remove(variable);
    }
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_GLOBAL", gitconfig)
        .env("GIT_CONFIG_NOSYSTEM", "1");
}

fn succeed(output: Output, what: &str) -> anyhow::Result<String> {
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        false => Err(anyhow::anyhow!("{what} failed: {}", String::from_utf8_lossy(&output.stderr))),
    }
}

pub fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))
}


// ===========
// === Run ===
// ===========

#[derive(Debug)]
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Run {
    pub fn ok(self) -> anyhow::Result<Self> {
        self.exits(0)
    }

    pub fn exits(self, code: i32) -> anyhow::Result<Self> {
        match self.code == code {
            true => Ok(self),
            false => Err(anyhow::anyhow!("expected exit {code}, got {self:#?}")),
        }
    }
}


// ===================
// === closed_port ===
// ===================

/// A local port nothing listens on.
pub fn closed_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}


// ==============
// === Server ===
// ==============

/// A TCP server on 127.0.0.1 that handles every connection until dropped; dropping it stops every thread.
pub struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    /// Accepts connections and never answers.
    pub fn silent() -> anyhow::Result<Self> {
        Self::start(|stream, stop| {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
            drop(stream);
        })
    }

    /// Answers every HTTP request with `401 Unauthorized`.
    pub fn unauthorized() -> anyhow::Result<Self> {
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

    pub fn url(&self) -> String {
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
