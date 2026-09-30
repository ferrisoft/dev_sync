use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;

use crate::git::failure;
use crate::process;


// =====================
// === NetworkPolicy ===
// =====================

/// Time limits, retries and parallelism for everything that runs git.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkPolicy {
    /// How long a network operation, or a local one that may take long, can go without printing anything before it
    /// is stopped. Git reports progress while a transfer moves, so a slow but working transfer takes as long as it
    /// needs; dropped connections are noticed much sooner by ssh and curl (`SSH_COMMAND`, `SLOW_TRANSFER_LIMITS`).
    pub(crate) stall_limit: Duration,
    /// For quick local commands: not network, but nothing may hang forever.
    pub(crate) local_timeout: Duration,
    /// Total attempts for a network failure; other failures are never retried.
    pub(crate) attempts: u32,
    /// The pause after the first failed attempt; later pauses are 2.5 times longer.
    pub(crate) retry_base: Duration,
    /// Network operations running at once.
    pub(crate) parallelism: usize,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            stall_limit: Duration::from_secs(300),
            local_timeout: Duration::from_secs(60),
            attempts: 3,
            retry_base: Duration::from_secs(2),
            parallelism: 8,
        }
    }
}

impl NetworkPolicy {
    /// The defaults, with `DEV_SYNC_NETWORK_TIMEOUT_SECS` (the stall limit) and `DEV_SYNC_RETRY_BASE_DELAY_MS` applied.
    pub(crate) fn from_env() -> anyhow::Result<Self> {
        Self::with_overrides(
            std::env::var_os("DEV_SYNC_NETWORK_TIMEOUT_SECS").as_deref(),
            std::env::var_os("DEV_SYNC_RETRY_BASE_DELAY_MS").as_deref(),
        )
    }

    /// The pause after failed attempt number `attempt` (counting from 1).
    pub(crate) fn retry_delay(&self, attempt: u32) -> Duration {
        match attempt {
            0 | 1 => self.retry_base,
            _ => self.retry_base.checked_mul(5).map_or(Duration::MAX, |delay| delay / 2),
        }
    }

    fn with_overrides(stall_secs: Option<&OsStr>, retry_base_ms: Option<&OsStr>) -> anyhow::Result<Self> {
        let stall = stall_secs.map(|value| positive("DEV_SYNC_NETWORK_TIMEOUT_SECS", value)).transpose()?;
        let base = retry_base_ms.map(|value| number("DEV_SYNC_RETRY_BASE_DELAY_MS", value)).transpose()?;
        let defaults = Self::default();
        Ok(Self {
            stall_limit: stall.map_or(defaults.stall_limit, Duration::from_secs),
            retry_base: base.map_or(defaults.retry_base, Duration::from_millis),
            ..defaults
        })
    }
}

fn number(name: &str, value: &OsStr) -> anyhow::Result<u64> {
    value
        .to_str()
        .and_then(|text| text.parse::<u64>().ok())
        .with_context(|| format!("{name} must be a whole number, not {value:?}"))
}

fn positive(name: &str, value: &OsStr) -> anyhow::Result<u64> {
    number(name, value).and_then(|number| match number {
        0 => Err(anyhow::anyhow!("{name} must be at least 1")),
        _ => Ok(number),
    })
}


// ===========
// === Git ===
// ===========

/// Runs the `git` command, so the user's ssh-agent, `~/.ssh/config`, credential helpers and `insteadOf` rules keep
/// working.
pub(crate) struct Git {
    policy: NetworkPolicy,
    /// Variables set for every git run, over the inherited environment.
    environment: Vec<(OsString, OsString)>,
}

impl Git {
    pub(crate) fn new(policy: NetworkPolicy) -> Self {
        Self { policy, environment: Vec::new() }
    }

    #[cfg(test)]
    pub(crate) fn with_environment(policy: NetworkPolicy, environment: Vec<(OsString, OsString)>) -> Self {
        Self { policy, environment }
    }

    pub(crate) fn policy(&self) -> &NetworkPolicy {
        &self.policy
    }

    /// Git on the repository at `dir`.
    pub(crate) fn at<'a>(&'a self, dir: &'a Path) -> Invocation<'a> {
        Invocation { git: self, place: Place::Repository(dir), args: Vec::new() }
    }

    /// Git in `dir`, on no repository: for `clone`.
    pub(crate) fn in_directory<'a>(&'a self, dir: &'a Path) -> Invocation<'a> {
        Invocation { git: self, place: Place::Directory(dir), args: Vec::new() }
    }

    /// Git wherever dev_sync runs, on no repository.
    pub(crate) fn outside(&self) -> Invocation<'_> {
        Invocation { git: self, place: Place::Anywhere, args: Vec::new() }
    }
}


// =============
// === Place ===
// =============

/// Where a git command runs, which decides the repository it acts on.
#[derive(Clone, Copy, Debug)]
enum Place<'a> {
    /// `git -C <dir> --git-dir=.git --work-tree=.`: the repository at `dir` and nothing else. Git never looks for one
    /// elsewhere, so a directory that lost its `.git` can't reach the workspace repository around it.
    Repository(&'a Path),
    Directory(&'a Path),
    Anywhere,
}

impl<'a> Place<'a> {
    fn dir(self) -> Option<&'a Path> {
        match self {
            Self::Repository(dir) | Self::Directory(dir) => Some(dir),
            Self::Anywhere => None,
        }
    }
}


// ==============
// === Access ===
// ==============

/// What a local command does, which decides its environment and time limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    /// Takes no optional locks, so queries can run in parallel with each other and with the user's git.
    Read,
    Write,
    /// May take long: it waits for the user (a commit-signing passphrase) or runs hooks and filters (a checkout, which
    /// may download LFS objects). Stopped only after the stall limit without any output.
    Lengthy,
}


// ===============
// === Prompts ===
// ===============

/// Whether git may ask the user something: ssh a passphrase or a host key, an askpass program or a credential helper
/// a password. Parallel operations forbid it, so interleaved hidden prompts can't hang.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Prompts {
    Allowed,
    Forbidden,
}


// ==================
// === Invocation ===
// ==================

/// Variables that point git at a specific repository. Inherited from, say, a git alias or hook, they would make every
/// `git -C <repo>` act on the wrong repository.
const REPOSITORY_VARIABLES: [&str; 11] = [
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
];

/// Gives up on a server that stopped answering after 2 minutes, which rides out a network blackout that long: a
/// transfer that is given up has to start over from scratch.
const SSH_COMMAND: &str = "ssh -o ConnectTimeout=20 -o ServerAliveInterval=30 -o ServerAliveCountMax=4";
/// The same for HTTPS: gives up after 2 minutes below 1 KB/s.
const SLOW_TRANSFER_LIMITS: [&str; 4] = ["-c", "http.lowSpeedLimit=1000", "-c", "http.lowSpeedTime=120"];

/// One git command being built.
pub(crate) struct Invocation<'a> {
    git: &'a Git,
    place: Place<'a>,
    args: Vec<OsString>,
}

#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    Local(Access),
    /// `ssh` replaces the ssh command, unless the user configured their own.
    Network { ssh: Option<&'a str>, prompts: Prompts },
}

impl Invocation<'_> {
    pub(crate) fn arg<S>(mut self, arg: S) -> Self where
    S: AsRef<OsStr> {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub(crate) fn args<I, S>(self, args: I) -> Self where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr> {
        args.into_iter().fold(self, Self::arg)
    }

    /// Runs a local command and returns its result whatever the exit code. Not finishing in time is an error.
    pub(crate) fn run(self, access: Access) -> anyhow::Result<process::Finished> {
        let limit = match access {
            Access::Read | Access::Write => process::Limit::Total(self.git.policy.local_timeout),
            Access::Lengthy => process::Limit::Silence(self.git.policy.stall_limit),
        };
        match self.execute(self.command(Kind::Local(access)), limit)? {
            process::Completion::Finished(finished) => Ok(finished),
            process::Completion::TimedOut { .. } => {
                let lock = self.leftover_lock().map(|lock| {
                    format!("; it left {} behind — remove it once no git runs there", lock.display())
                });
                let lock = lock.unwrap_or_default();
                let late = match limit {
                    process::Limit::Total(limit) => format!("did not finish within {} s", limit.as_secs()),
                    process::Limit::Silence(limit) => format!("made no progress for {} s", limit.as_secs()),
                };
                Err(anyhow::anyhow!("`{}` {late}{lock}", self.describe()))
            }
        }
    }

    /// Runs a local command that must succeed, and returns its stdout.
    pub(crate) fn run_ok(self, access: Access) -> anyhow::Result<Vec<u8>> {
        let description = self.describe();
        let finished = self.run(access)?;
        match finished.code {
            Some(0) => Ok(finished.stdout),
            code => Err(anyhow::anyhow!("`{description}` failed ({}): {}", exit(code), last_lines(&finished.stderr))),
        }
    }

    /// Runs a network command — the arguments start with `fetch`, `push` or `clone` — under the network policy,
    /// retrying network failures. Git is told to report progress, and the command is stopped only once it has shown
    /// none for the stall limit. A stall isn't retried: a connection that merely dropped is noticed sooner by ssh
    /// and curl, so a stall means something hangs.
    pub(crate) fn remote(self, prompts: Prompts) -> anyhow::Result<failure::RemoteOutcome> {
        self.remote_with_retry_prep(prompts, || Ok(()))
    }

    /// Like `remote`, running `before_retry` before every retry.
    pub(crate) fn remote_with_retry_prep<F>(
        self,
        prompts: Prompts,
        mut before_retry: F,
    ) -> anyhow::Result<failure::RemoteOutcome> where
    F: FnMut() -> anyhow::Result<()> {
        let policy = &self.git.policy;
        let silence = policy.stall_limit;
        let ssh = self.ssh_command(prompts)?;
        let mut attempt = 1_u32;
        loop {
            let command = self.command(Kind::Network { ssh: ssh.as_deref(), prompts });
            let completion = self.execute(command, process::Limit::Silence(silence))?;
            match judge(completion, attempt, silence) {
                failure::RemoteOutcome::Failed(failure)
                    if failure.kind == failure::RemoteFailureKind::Network && attempt < policy.attempts =>
                {
                    tracing::debug!(attempt, detail = %failure.detail, "network failure, retrying");
                    before_retry()?;
                    thread::sleep(policy.retry_delay(attempt));
                    attempt = attempt.saturating_add(1);
                }
                outcome => break Ok(outcome),
            }
        }
    }

    /// The command line, for messages: `git -C <dir> <args>`.
    fn describe(&self) -> String {
        let dir = self.place.dir().map(|dir| format!("-C {} ", dir.display())).unwrap_or_default();
        let args = self.args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>();
        format!("git {dir}{}", args.join(" "))
    }

    /// The index lock of the repository, if there is one: a killed git can leave it, and then every later command
    /// that writes there fails.
    fn leftover_lock(&self) -> Option<PathBuf> {
        match self.place {
            Place::Repository(dir) => Some(dir.join(".git").join("index.lock")).filter(|lock| lock.exists()),
            Place::Directory(_) | Place::Anywhere => None,
        }
    }

    fn command(&self, kind: Kind<'_>) -> Command {
        let mut command = Command::new("git");
        match self.place {
            Place::Repository(dir) => {
                command.arg("-C").arg(dir).args(["--git-dir=.git", "--work-tree=."]);
            }
            Place::Directory(dir) => {
                command.arg("-C").arg(dir);
            }
            Place::Anywhere => {}
        }
        for variable in REPOSITORY_VARIABLES {
            command.env_remove(variable);
        }
        command.envs(self.git.environment.iter().map(|(name, value)| (name, value)));
        command.env("LC_ALL", "C").env("GIT_TERMINAL_PROMPT", "0");
        match kind {
            Kind::Local(Access::Read) => {
                command.env("GIT_OPTIONAL_LOCKS", "0");
            }
            Kind::Local(Access::Write | Access::Lengthy) => {}
            Kind::Network { ssh, prompts } => {
                command.args(SLOW_TRANSFER_LIMITS);
                if let Some(ssh) = ssh {
                    command.env("GIT_SSH_COMMAND", ssh);
                }
                if prompts == Prompts::Forbidden {
                    command.env("GIT_ASKPASS", "").args(["-c", "credential.interactive=false"]);
                }
            }
        }
        match (kind, self.args.split_first()) {
            (Kind::Network { .. }, Some((subcommand, rest))) => {
                command.arg(subcommand).arg("--progress").args(rest);
            }
            (Kind::Network { .. } | Kind::Local(_), _) => {
                command.args(&self.args);
            }
        }
        command
    }

    fn execute(&self, command: Command, limit: process::Limit) -> anyhow::Result<process::Completion> {
        let started = Instant::now();
        let completion = process::run(command, limit);
        let status = match &completion {
            Ok(process::Completion::Finished(finished)) => exit(finished.code),
            Ok(process::Completion::TimedOut { .. }) => "stopped at its limit".to_owned(),
            Err(error) => format!("failed to start: {error}"),
        };
        tracing::debug!(command = %self.describe(), elapsed = ?started.elapsed(), %status, "git");
        completion
    }

    /// The ssh command for a network operation, unless the user configured their own through `GIT_SSH_COMMAND`,
    /// `GIT_SSH` or `core.sshCommand` (checked where the command runs, so a repository's own setting wins too).
    fn ssh_command(&self, prompts: Prompts) -> anyhow::Result<Option<String>> {
        let configured_in_env = ["GIT_SSH_COMMAND", "GIT_SSH"].iter().any(|name| std::env::var_os(name).is_some());
        let configured = configured_in_env || {
            let query = Invocation { git: self.git, place: self.place, args: Vec::new() };
            query.args(["config", "--get", "core.sshCommand"]).run(Access::Read)?.code == Some(0)
        };
        Ok((!configured).then(|| match prompts {
            Prompts::Allowed => SSH_COMMAND.to_owned(),
            Prompts::Forbidden => format!("{SSH_COMMAND} -o BatchMode=yes"),
        }))
    }
}

fn judge(completion: process::Completion, attempts: u32, silence: Duration) -> failure::RemoteOutcome {
    match completion {
        process::Completion::Finished(finished) if finished.code == Some(0) => {
            failure::RemoteOutcome::Succeeded(finished)
        }
        process::Completion::Finished(finished) => {
            let stderr = String::from_utf8_lossy(&finished.stderr);
            failure::RemoteOutcome::Failed(failure::RemoteFailure {
                kind: failure::classify(&stderr),
                attempts,
                detail: failure::detail(&stderr),
                stdout: finished.stdout,
            })
        }
        process::Completion::TimedOut { stderr } => failure::RemoteOutcome::Failed(failure::RemoteFailure {
            kind: failure::RemoteFailureKind::Stalled { silence },
            attempts,
            detail: failure::detail(&String::from_utf8_lossy(&stderr)),
            stdout: Vec::new(),
        }),
    }
}

fn exit(code: Option<i32>) -> String {
    code.map_or_else(|| "killed by a signal".to_owned(), |code| format!("exit {code}"))
}

/// The last few non-empty lines of `output`, joined.
fn last_lines(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
    let skip = lines.len().saturating_sub(5);
    lines.into_iter().skip(skip).collect::<Vec<_>>().join("; ")
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Duration;

    use crate::fixtures;
    use crate::git::failure;
    use crate::git::failure::RemoteFailureKind;
    use crate::git::failure::RemoteOutcome;
    use super::Access;
    use super::Git;
    use super::NetworkPolicy;
    use super::Prompts;

    fn failure_of(outcome: RemoteOutcome) -> anyhow::Result<failure::RemoteFailure> {
        match outcome {
            RemoteOutcome::Failed(failure) => Ok(failure),
            RemoteOutcome::Succeeded(finished) => anyhow::bail!("expected a failure, got {finished:?}"),
        }
    }

    #[test]
    fn a_query_returns_its_output() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet", "repo"])?;
        let repo = sandbox.path().join("repo");
        let output = fixtures::git().at(&repo).args(["rev-parse", "--is-inside-work-tree"]).run_ok(Access::Read)?;
        assert_eq!(output, b"true\n");
        let missing = fixtures::git().at(&repo).args(["config", "--get", "no.such-key"]).run(Access::Read)?;
        assert_eq!(missing.code, Some(1));
        Ok(())
    }

    #[test]
    fn tests_never_see_the_developer_s_git_configuration() -> anyhow::Result<()> {
        let listed = fixtures::git().outside().args(["config", "--list", "--show-scope"]).run_ok(Access::Read)?;
        let listed = String::from_utf8_lossy(&listed).into_owned();
        let foreign = listed.lines().filter(|line| line.starts_with("system") || line.starts_with("global"));
        assert_eq!(foreign.collect::<Vec<_>>(), Vec::<&str>::new());
        Ok(())
    }

    #[test]
    fn a_non_zero_exit_is_an_error_naming_the_command_and_directory() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet", "repo"])?;
        let repo = sandbox.path().join("repo");
        let error = fixtures::git().at(&repo).args(["rev-parse", "--verify", "no-such-ref"]).run_ok(Access::Read);
        let message = error.err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains(&format!("git -C {} rev-parse --verify no-such-ref", repo.display())), "{message}");
        Ok(())
    }

    #[test]
    fn never_escapes_into_an_enclosing_repository() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        for parent in [sandbox.path().join("plain"), sandbox.path().join("with:colon")] {
            std::fs::create_dir(&parent)?;
            sandbox.git(&parent, &["init", "--quiet"])?;
            let lost = parent.join("lost-its-git");
            std::fs::create_dir(&lost)?;
            let inside = fixtures::git().at(&lost).args(["rev-parse", "--git-dir"]).run(Access::Read)?;
            assert_ne!(inside.code, Some(0), "git found the enclosing repository: {inside:?}");
        }
        Ok(())
    }

    #[test]
    fn a_timeout_names_a_lock_git_left_behind() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet", "repo"])?;
        let repo = sandbox.path().join("repo");
        std::fs::write(repo.join(".git").join("index.lock"), "")?;
        let git = Git::with_environment(
            NetworkPolicy { local_timeout: Duration::from_millis(300), ..fixtures::git().policy().clone() },
            Vec::new(),
        );
        let slow = git.at(&repo).args(["-c", "alias.slow=!sleep 5", "slow"]).run(Access::Write);
        let message = slow.err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains("did not finish within 0 s"), "{message}");
        assert!(message.contains(&repo.join(".git").join("index.lock").display().to_string()), "{message}");
        Ok(())
    }

    #[test]
    fn a_closed_port_is_a_network_failure_after_three_attempts() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet"])?;
        let url = format!("http://127.0.0.1:{}/x.git", fixtures::closed_port()?);
        let outcome = fixtures::git().at(sandbox.path()).args(["fetch", &url]).remote(Prompts::Forbidden)?;
        let failure = failure_of(outcome)?;
        assert_eq!((failure.kind, failure.attempts), (RemoteFailureKind::Network, 3));
        Ok(())
    }

    #[test]
    fn a_missing_repository_is_not_found_after_one_attempt() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet"])?;
        let missing = sandbox.path().join("nope");
        let outcome = fixtures::git().at(sandbox.path()).arg("fetch").arg(&missing).remote(Prompts::Forbidden)?;
        let failure = failure_of(outcome)?;
        assert_eq!((failure.kind, failure.attempts), (RemoteFailureKind::NotFound, 1));
        Ok(())
    }

    #[test]
    fn a_silent_server_is_a_stall_and_is_not_retried() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet"])?;
        let server = fixtures::Server::silent()?;
        let silence = Duration::from_millis(700);
        let outcome = fixtures::git_with_stall_limit(silence)
            .at(sandbox.path())
            .args(["fetch", &server.url()])
            .remote(Prompts::Forbidden)?;
        let failure = failure_of(outcome)?;
        assert_eq!((failure.kind, failure.attempts), (RemoteFailureKind::Stalled { silence }, 1));
        Ok(())
    }

    #[test]
    fn network_commands_report_progress() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        let other = sandbox.clone(&remote, &sandbox.path().join("other"))?;
        sandbox.commit(&other, "a", "1")?;
        sandbox.git(&other, &["push", "--quiet"])?;
        match fixtures::git().at(&clone).args(["fetch"]).remote(Prompts::Forbidden)? {
            RemoteOutcome::Succeeded(finished) => {
                let stderr = String::from_utf8_lossy(&finished.stderr);
                assert!(stderr.contains("objects"), "no progress in {stderr:?}");
            }
            RemoteOutcome::Failed(failure) => anyhow::bail!("the fetch failed: {failure:?}"),
        }
        Ok(())
    }

    #[test]
    fn a_lengthy_command_may_run_while_it_prints() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet", "repo"])?;
        let repo = sandbox.path().join("repo");
        let busy = "alias.busy=!for step in 1 2 3 4 5 6 7 8; do echo . >&2; sleep 0.1; done";
        let git = fixtures::git_with_stall_limit(Duration::from_millis(400));
        let finished = git.at(&repo).args(["-c", busy, "busy"]).run(Access::Lengthy)?;
        assert_eq!(finished.code, Some(0));
        let silent = git.at(&repo).args(["-c", "alias.quiet=!sleep 5", "quiet"]).run(Access::Lengthy);
        let message = silent.err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains("made no progress for 0 s"), "{message}");
        Ok(())
    }

    #[test]
    fn an_http_401_is_an_auth_failure_after_one_attempt() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet"])?;
        let server = fixtures::Server::unauthorized()?;
        let outcome = fixtures::git().at(sandbox.path()).args(["fetch", &server.url()]).remote(Prompts::Forbidden)?;
        let failure = failure_of(outcome)?;
        assert_eq!((failure.kind, failure.attempts), (RemoteFailureKind::Auth, 1));
        Ok(())
    }

    #[test]
    fn a_fetch_that_may_not_prompt_never_runs_an_askpass_program() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        sandbox.git(sandbox.path(), &["init", "--quiet"])?;
        let asked = sandbox.path().join("asked");
        let askpass = sandbox.path().join("askpass");
        std::fs::write(&askpass, format!("#!/bin/sh\ntouch '{}'\n", asked.display()))?;
        std::fs::set_permissions(&askpass, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        sandbox.git(sandbox.path(), &["config", "core.askPass", &askpass.to_string_lossy()])?;
        let server = fixtures::Server::unauthorized()?;
        let outcome = fixtures::git().at(sandbox.path()).args(["fetch", &server.url()]).remote(Prompts::Forbidden)?;
        assert_eq!(failure_of(outcome)?.kind, RemoteFailureKind::Auth);
        assert!(!asked.exists(), "the askpass program ran");
        Ok(())
    }

    #[test]
    fn a_local_fetch_succeeds() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        let outcome = fixtures::git().at(&clone).arg("fetch").remote(Prompts::Forbidden)?;
        assert!(matches!(outcome, RemoteOutcome::Succeeded(_)), "{outcome:?}");
        Ok(())
    }

    #[test]
    fn policy_reads_overrides_and_rejects_nonsense() -> anyhow::Result<()> {
        let defaults = NetworkPolicy::with_overrides(None, None)?;
        assert_eq!(defaults.stall_limit, Duration::from_secs(300));
        assert_eq!(defaults.local_timeout, Duration::from_secs(60));
        assert_eq!(defaults.attempts, 3);
        assert_eq!(defaults.retry_delay(1), Duration::from_secs(2));
        assert_eq!(defaults.retry_delay(2), Duration::from_secs(5));
        let custom = NetworkPolicy::with_overrides(Some(OsStr::new("2")), Some(OsStr::new("10")))?;
        assert_eq!(custom.stall_limit, Duration::from_secs(2));
        assert_eq!(custom.retry_delay(1), Duration::from_millis(10));
        assert_eq!(custom.retry_delay(2), Duration::from_millis(25));
        assert!(NetworkPolicy::with_overrides(Some(OsStr::new("soon")), None).is_err());
        assert!(NetworkPolicy::with_overrides(Some(OsStr::new("0")), None).is_err());
        assert!(NetworkPolicy::with_overrides(None, Some(OsStr::new("-1"))).is_err());
        Ok(())
    }
}
