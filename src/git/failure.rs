use std::fmt;
use std::time::Duration;

use crate::process;


// =========================
// === RemoteFailureKind ===
// =========================

/// Why a fetch, push or clone failed. Only `Network` failures are retried.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteFailureKind {
    Network,
    /// Stopped after showing no progress for `silence`.
    Stalled { silence: Duration },
    Auth,
    NotFound,
    Rejected,
    Other,
}

impl RemoteFailureKind {
    fn hint(self) -> Option<String> {
        match self {
            Self::Network => Some("check the connection; running it again retries".to_owned()),
            Self::Stalled { silence } => Some(format!(
                "it made no progress for {} s; check the connection, then run it again",
                silence.as_secs()
            )),
            Self::Auth => Some(
                "check ssh-agent (`ssh-add -l`) and access to the repo; test with `ssh -T git@github.com`".to_owned(),
            ),
            Self::NotFound => Some("the repo or branch doesn't exist, or you lack access".to_owned()),
            Self::Rejected => Some("the remote has commits you don't have — pull first".to_owned()),
            Self::Other => None,
        }
    }
}

impl fmt::Display for RemoteFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Network => "network",
            Self::Stalled { .. } => "stalled",
            Self::Auth => "auth",
            Self::NotFound => "not found",
            Self::Rejected => "rejected",
            Self::Other => "error",
        })
    }
}


// =====================
// === RemoteFailure ===
// =====================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteFailure {
    pub(crate) kind: RemoteFailureKind,
    pub(crate) attempts: u32,
    /// The stderr line that explains the failure.
    pub(crate) detail: String,
    /// What the command printed on stdout, e.g. `push --porcelain` ref statuses of a partly rejected push.
    pub(crate) stdout: Vec<u8>,
}

impl RemoteFailure {
    /// Whether git got through to the remote and failed at something smaller, such as one ref it wouldn't update.
    pub(crate) fn reached_the_remote(&self) -> bool {
        match self.kind {
            RemoteFailureKind::Rejected | RemoteFailureKind::Other => true,
            RemoteFailureKind::Network
            | RemoteFailureKind::Stalled { .. }
            | RemoteFailureKind::Auth
            | RemoteFailureKind::NotFound => false,
        }
    }

    /// The failure on one line, e.g. `fetch failed (network, 3 attempts): <detail> — <hint>`.
    pub(crate) fn describe(&self, operation: &str) -> String {
        let attempts = match self.attempts {
            0 | 1 => String::new(),
            many => format!(", {many} attempts"),
        };
        let detail = match self.detail.as_str() {
            "" => String::new(),
            detail => format!(": {detail}"),
        };
        let hint = self.kind.hint().map(|hint| format!(" — {hint}")).unwrap_or_default();
        format!("{operation} failed ({}{attempts}){detail}{hint}", self.kind)
    }
}


// =====================
// === RemoteOutcome ===
// =====================

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum RemoteOutcome {
    Succeeded(process::Finished),
    Failed(RemoteFailure),
}


// ======================
// === Classification ===
// ======================

const REJECTED: &[&str] = &[
    "[rejected]",
    "[remote rejected]",
    "non-fast-forward",
    "fetch first",
    "updates were rejected",
];

const AUTH: &[&str] = &[
    "permission denied (publickey",
    "permission denied, please try again",
    "authentication failed",
    "could not read username",
    "could not read password",
    "terminal prompts disabled",
    "host key verification failed",
    "returned error: 401",
    "returned error: 403",
    "invalid username or password",
    "denied to",
    "saml sso",
    "unable to get password from user",
];

const NOT_FOUND: &[&str] = &[
    "repository not found",
    "does not appear to be a git repository",
    "does not exist",
    "returned error: 404",
    "couldn't find remote ref",
    "no such remote",
];

const NETWORK: &[&str] = &[
    "could not resolve host",
    "could not resolve hostname",
    "temporary failure in name resolution",
    "name or service not known",
    "network is unreachable",
    "no route to host",
    "connection timed out",
    "connection refused",
    "connection reset",
    "operation timed out",
    "failed to connect to",
    "couldn't connect to server",
    "the remote end hung up unexpectedly",
    "early eof",
    "rpc failed",
    "unexpected disconnect",
    "connection closed by",
    "kex_exchange_identification",
    "broken pipe",
    "ssl_error",
    "gnutls",
    "tls connection",
    "operation too slow",
    "returned error: 5",
];

/// Lines git adds to most failures, which never say what went wrong.
const BOILERPLATE: [&str; 3] = [
    "fatal: Could not read from remote repository.",
    "Please make sure you have the correct access rights",
    "and the repository exists.",
];

/// How git and hosting services start the line that says what went wrong.
const PREFIXES: [&str; 3] = ["ERROR:", "fatal:", "error:"];

const RULES: [Rule; 4] = [
    Rule { kind: RemoteFailureKind::Rejected, needles: REJECTED },
    Rule { kind: RemoteFailureKind::Auth, needles: AUTH },
    Rule { kind: RemoteFailureKind::NotFound, needles: NOT_FOUND },
    Rule { kind: RemoteFailureKind::Network, needles: NETWORK },
];

struct Rule {
    kind: RemoteFailureKind,
    needles: &'static [&'static str],
}

impl Rule {
    fn matches(&self, text: &str) -> bool {
        let lowercase = text.to_lowercase();
        self.needles.iter().any(|needle| lowercase.contains(needle))
    }
}

/// Classifies git's stderr, produced under `LC_ALL=C`. Rules are checked in order and the first match wins.
pub(crate) fn classify(stderr: &str) -> RemoteFailureKind {
    RULES.iter().find(|rule| rule.matches(stderr)).map_or(RemoteFailureKind::Other, |rule| rule.kind)
}

/// The stderr line that explains the failure: the first line matching the winning rule, else the first `ERROR:`,
/// `fatal:` or `error:` line, else the first line, leaving out noise (git's boilerplate and hints, progress
/// reports) when anything else is there. With nothing but noise, the last line: where a stalled transfer stopped.
/// Each line is read as a terminal shows it, keeping only what follows its last `\r`. The prefix is dropped.
pub(crate) fn detail(stderr: &str) -> String {
    let shown = process::shown_lines(stderr).collect::<Vec<_>>();
    let telling = shown.iter().copied().filter(|line| !is_noise(line)).collect::<Vec<_>>();
    let rule = RULES.iter().find(|rule| rule.matches(stderr));
    let explaining = flagged(rule, &telling)
        .or_else(|| telling.first().copied())
        .or_else(|| flagged(rule, &shown))
        .or_else(|| shown.last().copied());
    explaining
        .map(|line| PREFIXES.iter().find_map(|prefix| line.strip_prefix(prefix)).unwrap_or(line).trim().to_owned())
        .unwrap_or_default()
}

/// The first line matching `rule`, else the first `ERROR:`, `fatal:` or `error:` line.
fn flagged<'a>(rule: Option<&Rule>, lines: &[&'a str]) -> Option<&'a str> {
    let matching = rule.and_then(|rule| lines.iter().find(|line| rule.matches(line)));
    matching.or_else(|| lines.iter().find(|line| PREFIXES.iter().any(|prefix| line.starts_with(prefix)))).copied()
}

/// A line that never says what went wrong: git's boilerplate and hints, and its progress reports.
fn is_noise(line: &str) -> bool {
    BOILERPLATE.contains(&line)
        || line.starts_with("hint:")
        || line.starts_with("Cloning into ")
        || line.starts_with("remote: Total ")
        || line.starts_with("Total ")
        || line.contains("% (")
        || line.ends_with(", done.")
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::RemoteFailure;
    use super::RemoteFailureKind;
    use super::classify;
    use super::detail;

    #[test]
    fn classifies_real_stderr_samples() {
        let samples = [
            (
                "ssh: Could not resolve hostname github.com: Temporary failure in name resolution\n\
                 fatal: Could not read from remote repository.\n\n\
                 Please make sure you have the correct access rights\nand the repository exists.\n",
                RemoteFailureKind::Network,
            ),
            (
                "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.\n",
                RemoteFailureKind::Auth,
            ),
            (
                "ERROR: Repository not found.\nfatal: Could not read from remote repository.\n",
                RemoteFailureKind::NotFound,
            ),
            (
                "fatal: unable to access 'http://127.0.0.1:9/x.git/': Failed to connect to \
                 127.0.0.1 port 9 after 0 ms: Couldn't connect to server\n",
                RemoteFailureKind::Network,
            ),
            (
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n",
                RemoteFailureKind::Auth,
            ),
            (
                " ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs\n",
                RemoteFailureKind::Rejected,
            ),
            ("fatal: '/tmp/nope' does not appear to be a git repository\n", RemoteFailureKind::NotFound),
            ("fatal: the remote end hung up unexpectedly\n", RemoteFailureKind::Network),
            ("Host key verification failed.\nfatal: Could not read from remote repository.\n", RemoteFailureKind::Auth),
            ("fatal: couldn't find remote ref refs/heads/nope\n", RemoteFailureKind::NotFound),
            ("fatal: repository '/tmp/no-such-remote' does not exist\n", RemoteFailureKind::NotFound),
            (
                "error: RPC failed; curl 56 GnuTLS recv error (-54): Error in the pull function.\n",
                RemoteFailureKind::Network,
            ),
            ("fatal: something unexpected happened\n", RemoteFailureKind::Other),
            ("", RemoteFailureKind::Other),
            (
                "ERROR: Permission to ferrisoft/website.git denied to someone.\nfatal: Could not read from remote \
                 repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n",
                RemoteFailureKind::Auth,
            ),
            (
                "ERROR: The `ferrisoft' organization has enabled or enforced SAML SSO. To access this repository, \
                 you must re-authorize the SSH key.\nfatal: Could not read from remote repository.\n",
                RemoteFailureKind::Auth,
            ),
            ("error: unable to get password from user\nfatal: could not read Username\n", RemoteFailureKind::Auth),
            (
                "error: RPC failed; curl 28 Operation too slow. Less than 1000 bytes/sec transferred the last 60 \
                 seconds\n",
                RemoteFailureKind::Network,
            ),
            (
                "fatal: unable to access 'https://example.com/x.git/': Operation too slow. Less than 1000 bytes/sec \
                 transferred the last 60 seconds\n",
                RemoteFailureKind::Network,
            ),
            (
                "fatal: unable to access 'https://x/': The requested URL returned error: 502\n",
                RemoteFailureKind::Network,
            ),
            (
                "fatal: unable to access 'https://x/': The requested URL returned error: 403\n",
                RemoteFailureKind::Auth,
            ),
        ];
        for (stderr, kind) in samples {
            assert_eq!(classify(stderr), kind, "{stderr:?}");
        }
    }

    #[test]
    fn detail_skips_git_s_boilerplate() {
        let trailer = "fatal: Could not read from remote repository.\n\nPlease make sure you have the correct access \
                       rights\nand the repository exists.\n";
        let denied = format!("ERROR: Permission to ferrisoft/website.git denied to someone.\n{trailer}");
        assert_eq!(detail(&denied), "Permission to ferrisoft/website.git denied to someone.");
        let unknown = format!("Bad owner or permissions on /home/someone/.ssh/config\n{trailer}");
        assert_eq!(detail(&unknown), "Bad owner or permissions on /home/someone/.ssh/config");
        assert_eq!(detail(trailer), "Could not read from remote repository.");
    }

    #[test]
    fn detail_skips_progress() {
        let progress = "Cloning into 'x'...\nremote: Enumerating objects: 3, done.\nremote: Counting objects:  33% \
                        (1/3)\rremote: Counting objects: 100% (3/3), done.\nReceiving objects:  33% (1/3)\r\
                        Receiving objects:  66% (2/3)\r";
        assert_eq!(detail(&format!("{progress}something odd happened\n")), "something odd happened");
        assert_eq!(detail(progress), "Receiving objects:  66% (2/3)");
    }

    #[test]
    fn auth_wins_over_not_found_and_not_found_over_network() {
        let auth_and_network = "fatal: Authentication failed for 'https://x/'\n\
                                fatal: the remote end hung up unexpectedly\n";
        assert_eq!(classify(auth_and_network), RemoteFailureKind::Auth);
        let missing_and_network = "ERROR: Repository not found.\nfatal: the remote end hung up unexpectedly\n";
        assert_eq!(classify(missing_and_network), RemoteFailureKind::NotFound);
        let auth_and_missing = "remote: Invalid username or password.\nfatal: repository 'https://x/' not found\n";
        assert_eq!(classify(auth_and_missing), RemoteFailureKind::Auth);
    }

    #[test]
    fn rejection_wins_over_everything_else() {
        let stderr = "error: failed to push some refs to 'x'\nhint: Updates were rejected because the tip of your \
                      current branch is behind\nfatal: the remote end hung up unexpectedly\n";
        assert_eq!(classify(stderr), RemoteFailureKind::Rejected);
    }

    #[test]
    fn detail_is_the_line_that_explains_the_failure() {
        let stderr = "ssh: Could not resolve hostname github.com: Temporary failure in name resolution\n\
                      fatal: Could not read from remote repository.\n\n\
                      Please make sure you have the correct access rights\nand the repository exists.\n";
        assert_eq!(detail(stderr), "ssh: Could not resolve hostname github.com: Temporary failure in name resolution");
        assert_eq!(detail("fatal: something odd\n\n"), "something odd");
        assert_eq!(detail(""), "");
    }

    #[test]
    fn describes_failures_with_kind_attempts_and_hint() {
        let failure = |kind, attempts, detail: &str| RemoteFailure {
            kind,
            attempts,
            detail: detail.to_owned(),
            stdout: vec![],
        };
        assert_eq!(
            failure(RemoteFailureKind::Network, 3, "Could not resolve host: github.com").describe("fetch"),
            "fetch failed (network, 3 attempts): Could not resolve host: github.com — check the connection; running \
             it again retries"
        );
        let auth = failure(RemoteFailureKind::Auth, 1, "Permission denied (publickey).").describe("clone");
        assert!(auth.starts_with("clone failed (auth): Permission denied (publickey). — check ssh-agent"), "{auth}");
        let silence = Duration::from_secs(300);
        let stalled = failure(RemoteFailureKind::Stalled { silence }, 1, "").describe("push");
        assert_eq!(
            stalled,
            "push failed (stalled) — it made no progress for 300 s; check the connection, then run it again"
        );
        assert_eq!(failure(RemoteFailureKind::Other, 1, "odd").describe("fetch"), "fetch failed (error): odd");
    }
}
