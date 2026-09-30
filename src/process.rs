//! Child processes with a deadline. Knows nothing about git.

use std::io::ErrorKind;
use std::io::Read;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;


// ================
// === Finished ===
// ================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Finished {
    /// `None` when a signal ended the process.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl Finished {
    /// `exit 1`, or `killed by a signal`.
    pub(crate) fn exit(&self) -> String {
        self.code.map_or_else(|| "killed by a signal".to_owned(), |code| format!("exit {code}"))
    }

    /// Stderr on one line, as a terminal shows it.
    pub(crate) fn error_text(&self) -> String {
        shown_lines(&String::from_utf8_lossy(&self.stderr)).collect::<Vec<_>>().join("; ")
    }
}


// ===================
// === shown_lines ===
// ===================

/// Each line of `text` as a terminal shows it: what follows its last `\r` (progress reports rewrite their line),
/// trimmed. Blank lines are left out.
pub(crate) fn shown_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|line| line.rsplit('\r').map(str::trim).find(|part| !part.is_empty()))
}


// ==================
// === Completion ===
// ==================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Completion {
    Finished(Finished),
    /// Stopped at its `Limit`. Holds the error output collected until then.
    TimedOut { stderr: Vec<u8> },
}


// ===========
// === run ===
// ===========

/// How long to keep collecting output after the child is gone. A killed git can leave an ssh child holding the pipes
/// open, so collection never waits for end-of-file without a bound.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// How long a child past its deadline gets between SIGTERM and SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);
const FIRST_PAUSE: Duration = Duration::from_millis(1);
const LONGEST_PAUSE: Duration = Duration::from_millis(25);

/// When a child is stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Limit {
    /// Once this long has passed since it started.
    Total(Duration),
    /// Once it has printed nothing for this long: a child that keeps reporting progress may take as long as it needs.
    Silence(Duration),
}

/// Runs `command` with stdin closed and its output captured, stopping it at `limit`. Output of any size is drained
/// while the child runs, so a chatty child can't block on a full pipe.
pub(crate) fn run(mut command: Command, limit: Limit) -> anyhow::Result<Completion> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| anyhow::anyhow!("failed to run {program}: {error} — is {program} installed?"))?;
    let (sender, receiver) = mpsc::channel();
    let started = start_reader(child.stdout.take(), Stream::Stdout, sender.clone())
        .and_then(|()| start_reader(child.stderr.take(), Stream::Stderr, sender));
    let watched = match started {
        Ok(()) => watch(&mut child, &receiver, limit),
        Err(error) => Err(abandon(&mut child, error)),
    }?;
    let output = collect(&receiver, watched.output);
    Ok(match watched.ending {
        Waited::Exited(status) => {
            Completion::Finished(Finished { code: status.code(), stdout: output.stdout, stderr: output.stderr })
        }
        Waited::Killed => Completion::TimedOut { stderr: output.stderr },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stream {
    Stdout,
    Stderr,
}

enum Chunk {
    Data { stream: Stream, bytes: Vec<u8> },
    End,
}

enum Waited {
    Exited(ExitStatus),
    Killed,
}

struct Watched {
    ending: Waited,
    output: Output,
}

/// What the readers delivered so far.
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Streams not at end-of-file yet.
    open: u8,
}

impl Output {
    /// Adds `chunk`; true when it carried data.
    fn take(&mut self, chunk: Chunk) -> bool {
        match chunk {
            Chunk::Data { stream, bytes } => {
                let heard = !bytes.is_empty();
                match stream {
                    Stream::Stdout => self.stdout.extend(bytes),
                    Stream::Stderr => self.stderr.extend(bytes),
                }
                heard
            }
            Chunk::End => {
                self.open = self.open.saturating_sub(1);
                false
            }
        }
    }
}

/// Reads a pipe until end-of-file on its own thread, sending each chunk as it arrives. The thread is detached: it ends
/// at end-of-file, or once nobody listens any more.
fn start_reader<R>(pipe: Option<R>, stream: Stream, sender: mpsc::Sender<Chunk>) -> anyhow::Result<()> where
R: Read + Send + 'static {
    let name = format!("{stream:?} reader").to_lowercase();
    let read_all = move || {
        let mut buffer = [0_u8; 8192];
        let mut pipe = pipe;
        while let Some(reader) = pipe.as_mut() {
            let bytes = match reader.read(&mut buffer) {
                Ok(0) => None,
                Ok(count) => buffer.get(..count).map(<[u8]>::to_vec),
                Err(error) if error.kind() == ErrorKind::Interrupted => Some(Vec::new()),
                Err(_) => None,
            };
            let delivered = bytes.is_some_and(|bytes| sender.send(Chunk::Data { stream, bytes }).is_ok());
            if !delivered {
                pipe = None;
            }
        }
        sender.send(Chunk::End).ok();
    };
    thread::Builder::new()
        .name(name)
        .spawn(read_all)
        .map(|_detached| ())
        .context("failed to start a thread to read a child process's output")
}

/// Waits for the child while gathering its output, until it exits or `limit` stops it. A child that exits right at
/// its deadline counts as finished.
fn watch(child: &mut Child, receiver: &mpsc::Receiver<Chunk>, limit: Limit) -> anyhow::Result<Watched> {
    let started = Instant::now();
    let mut heard = started;
    let mut output = Output { stdout: Vec::new(), stderr: Vec::new(), open: 2 };
    let mut pause = FIRST_PAUSE;
    loop {
        if let Some(status) = child.try_wait().context("failed to wait for a child process")? {
            break Ok(Watched { ending: Waited::Exited(status), output });
        }
        let deadline = match limit {
            Limit::Total(limit) => started.checked_add(limit),
            Limit::Silence(limit) => heard.checked_add(limit),
        };
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            break stop(child).map(|ending| Watched { ending, output });
        }
        let wait = remaining.map_or(pause, |remaining| remaining.min(pause));
        let received = match output.open {
            0 => {
                thread::sleep(wait);
                None
            }
            _ => receiver.recv_timeout(wait).ok(),
        };
        match received.map(|chunk| output.take(chunk)) {
            Some(true) => heard = Instant::now(),
            Some(false) => {}
            None => pause = pause.saturating_mul(2).min(LONGEST_PAUSE),
        }
    }
}

/// The child's exit status, or `None` once `deadline` passes first (never, without a deadline).
fn wait_until(child: &mut Child, deadline: Option<Instant>) -> anyhow::Result<Option<ExitStatus>> {
    let mut pause = FIRST_PAUSE;
    loop {
        if let Some(status) = child.try_wait().context("failed to wait for a child process")? {
            break Ok(Some(status));
        }
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining == Some(Duration::ZERO) {
            break Ok(None);
        }
        thread::sleep(remaining.map_or(pause, |remaining| remaining.min(pause)));
        pause = pause.saturating_mul(2).min(LONGEST_PAUSE);
    }
}

/// Stops a child past its deadline, with everything it started: SIGTERM first, so each can clean up (git removes its
/// lock files, ssh restores the terminal it prompts on), then SIGKILL for the child if it is still there after
/// `TERM_GRACE`. Only the child gets SIGKILL: it can't be gone and its id reused before it is reaped, which isn't
/// true of its descendants.
fn stop(child: &mut Child) -> anyhow::Result<Waited> {
    let family = family(child.id());
    tracing::debug!(?family, "stopping a child process past its deadline");
    for pid in family {
        if let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) {
            rustix::process::kill_process(pid, rustix::process::Signal::TERM).ok();
        }
    }
    if wait_until(child, Instant::now().checked_add(TERM_GRACE))?.is_none() {
        child.kill().context("failed to kill a timed-out child process")?;
        child.wait().context("failed to wait for a killed child process")?;
    }
    Ok(Waited::Killed)
}

/// `pid` and its descendants, parents first, from `/proc/<pid>/task/<tid>/children`; just `pid` where that isn't
/// available.
fn family(pid: u32) -> Vec<u32> {
    let mut family = vec![pid];
    let mut next = 0_usize;
    while let Some(member) = family.get(next).copied() {
        family.extend(children(member));
        next = next.saturating_add(1);
    }
    family
}

fn children(pid: u32) -> Vec<u32> {
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().filter_map(Result::ok);
    tasks
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|listed| listed.split_whitespace().filter_map(|word| word.parse().ok()).collect::<Vec<_>>())
        .collect()
}

/// Kills and reaps a child whose output can't be read, so it doesn't outlive the failure.
fn abandon(child: &mut Child, error: anyhow::Error) -> anyhow::Error {
    let cleanup = child.kill().and_then(|()| child.wait().map(|_| ()));
    if let Err(cleanup_error) = cleanup {
        tracing::debug!(%cleanup_error, "failed to stop a child process after an error");
    }
    error
}

/// The rest of the output, once the child is gone.
fn collect(receiver: &mpsc::Receiver<Chunk>, output: Output) -> Output {
    let deadline = Instant::now().checked_add(OUTPUT_GRACE);
    let mut output = output;
    while output.open > 0 {
        let remaining = deadline.map_or(OUTPUT_GRACE, |deadline| deadline.saturating_duration_since(Instant::now()));
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => {
                output.take(chunk);
            }
            Err(_) => output.open = 0,
        }
    }
    output
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use super::Completion;
    use super::Finished;
    use super::Limit;
    use super::run;
    use super::shown_lines;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn a_quick_command_finishes_with_its_code_and_output() -> anyhow::Result<()> {
        let completion = run(shell("printf out; printf err >&2; exit 3"), Limit::Total(Duration::from_secs(10)))?;
        assert_eq!(
            completion,
            Completion::Finished(Finished { code: Some(3), stdout: b"out".to_vec(), stderr: b"err".to_vec() })
        );
        Ok(())
    }

    #[test]
    fn a_slow_command_times_out_quickly() -> anyhow::Result<()> {
        let started = Instant::now();
        let completion = run(shell("echo started >&2; exec sleep 5"), Limit::Total(Duration::from_millis(200)))?;
        assert!(started.elapsed() < Duration::from_secs(1), "took {:?}", started.elapsed());
        assert_eq!(completion, Completion::TimedOut { stderr: b"started\n".to_vec() });
        Ok(())
    }

    #[test]
    fn a_command_that_keeps_printing_runs_past_a_silence_limit() -> anyhow::Result<()> {
        let script = "for step in 1 2 3 4 5 6 7 8; do printf . >&2; sleep 0.1; done; echo finished";
        let completion = run(shell(script), Limit::Silence(Duration::from_millis(400)))?;
        let finished = Finished { code: Some(0), stdout: b"finished\n".to_vec(), stderr: b"........".to_vec() };
        assert_eq!(completion, Completion::Finished(finished));
        Ok(())
    }

    #[test]
    fn a_command_that_goes_quiet_is_stopped_at_a_silence_limit() -> anyhow::Result<()> {
        let started = Instant::now();
        let script = "printf . >&2; sleep 0.2; printf . >&2; exec sleep 5";
        let completion = run(shell(script), Limit::Silence(Duration::from_millis(400)))?;
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        assert_eq!(completion, Completion::TimedOut { stderr: b"..".to_vec() });
        Ok(())
    }

    #[test]
    fn a_timed_out_command_gets_to_clean_up() -> anyhow::Result<()> {
        let script = "trap 'echo cleaned up >&2; exit 3' TERM; echo started >&2; sleep 5 & wait";
        let completion = run(shell(script), Limit::Total(Duration::from_millis(300)))?;
        assert_eq!(completion, Completion::TimedOut { stderr: b"started\ncleaned up\n".to_vec() });
        Ok(())
    }

    #[test]
    fn a_timed_out_command_takes_what_it_started_along() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let noted = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", noted.display());
        let completion = run(shell(&script), Limit::Total(Duration::from_millis(300)))?;
        assert!(matches!(completion, Completion::TimedOut { .. }), "{completion:?}");
        let grandchild = std::fs::read_to_string(&noted)?.trim().parse::<i32>()?;
        let alive = || {
            let stat = std::fs::read_to_string(format!("/proc/{grandchild}/stat")).unwrap_or_default();
            !stat.is_empty() && !stat.contains(") Z ")
        };
        let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap_or_else(Instant::now);
        while alive() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let survived = alive();
        if let Some(pid) = rustix::process::Pid::from_raw(grandchild).filter(|_| survived) {
            rustix::process::kill_process(pid, rustix::process::Signal::KILL).ok();
        }
        assert!(!survived, "the grandchild {grandchild} outlived the timeout");
        Ok(())
    }

    #[test]
    fn a_grandchild_holding_the_pipes_does_not_block_forever() -> anyhow::Result<()> {
        let started = Instant::now();
        let completion = run(shell("sleep 4 & echo done"), Limit::Total(Duration::from_secs(10)))?;
        assert!(started.elapsed() < Duration::from_millis(3500), "took {:?}", started.elapsed());
        match completion {
            Completion::Finished(finished) => assert_eq!(finished.stdout, b"done\n"),
            Completion::TimedOut { .. } => anyhow::bail!("the shell itself finished at once"),
        }
        Ok(())
    }

    #[test]
    fn a_mebibyte_of_output_does_not_deadlock() -> anyhow::Result<()> {
        let script = "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2";
        match run(shell(script), Limit::Total(Duration::from_secs(20)))? {
            Completion::Finished(finished) => {
                assert_eq!(finished.stdout.len(), 1_048_576);
                assert_eq!(finished.stderr.len(), 1_048_576);
            }
            Completion::TimedOut { .. } => anyhow::bail!("deadlocked"),
        }
        Ok(())
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let error = run(Command::new("dev-sync-no-such-program"), Limit::Total(Duration::from_secs(1))).err();
        let message = error.map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains("failed to run dev-sync-no-such-program"), "{message}");
        assert!(message.contains("is dev-sync-no-such-program installed?"), "{message}");
    }

    #[test]
    fn lines_read_as_a_terminal_shows_them() {
        let text = "Receiving objects:  50% (1/2)\rReceiving objects: 100% (2/2), done.\r\n\n  fatal: gone  \n\
                    left\r\r\n";
        let shown = shown_lines(text).collect::<Vec<_>>();
        assert_eq!(shown, ["Receiving objects: 100% (2/2), done.", "fatal: gone", "left"]);
        let finished = Finished { code: Some(1), stdout: Vec::new(), stderr: text.as_bytes().to_vec() };
        assert_eq!(finished.error_text(), "Receiving objects: 100% (2/2), done.; fatal: gone; left");
    }
}
