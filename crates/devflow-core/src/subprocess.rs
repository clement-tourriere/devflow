//! A command run under a deadline, with its resources bounded.
//!
//! Used for the commands whose answer devflow reads and waits for: pitchfork's
//! queries and its `start` and `stop`, Docker's endpoint discovery and the
//! `docker` calls of the container services. The semantics, all of them
//! bounded:
//!
//! - **Time.** The command runs in its own process group. When the deadline
//!   passes, it and everything it started are ended and reaped
//!   ([`crate::hooks::terminate_group`]: TERM, then KILL). `timed_out` says so.
//! - **Output.** Each stream is read as it arrives, on a thread, and at most
//!   [`Limits::max_output`] bytes of it (4 MiB by default) are kept. Beyond
//!   that the stream is still read, so the command is never blocked on a full
//!   pipe, but what it prints is dropped and `truncated` is set: the text is a
//!   prefix, and a caller that parses it (JSON) must treat that as an error.
//!   The failure `message` is taken from the last lines, which are kept apart.
//! - **After the process is gone.** Its output pipes can still be held by
//!   something it started that outlived it. The wait for them is bounded by
//!   [`DRAIN`] and by what is left of the timeout, whichever is shorter, one
//!   wait for both streams. What happens to such a process is the caller's
//!   choice ([`Leftovers`]): a query's strays are ended (the whole process
//!   group, TERM then KILL), because nothing a query starts should outlive it;
//!   an effect's are left alone, because `pitchfork start` may have launched
//!   the supervisor in its group.
//! - **Threads.** A reader that saw its stream end is joined; one whose pipe
//!   is still held (a process that escaped the group, or a [`Leftovers::Leave`]
//!   stray) is detached, and ends when that process closes the pipe. Input
//!   is written from a detached thread that ends with the pipe.
use anyhow::Result;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long to wait for output after the process is gone: a grandchild that
/// inherited the pipes must not hold the caller.
const DRAIN: Duration = Duration::from_millis(500);
/// The least that wait is given when the timeout is already used up, so that
/// output the command wrote just before it exited is not lost.
const MIN_DRAIN: Duration = Duration::from_millis(50);
/// How long processes left in the group get to leave after TERM, before KILL.
const STRAY_GRACE: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(20);
/// The most of one stream that is kept, by default.
pub(crate) const DEFAULT_MAX_OUTPUT: usize = 4 << 20;
/// The end of a truncated stream that is kept for the failure message.
const TAIL: usize = 8 << 10;

/// What to do with a process the command left behind, still holding its
/// output, once the command itself is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leftovers {
    /// End the process group (TERM, then KILL). For commands that start
    /// nothing meant to outlive them.
    Kill,
    /// Leave it. For commands that may start a long-lived process in their
    /// group on purpose (`pitchfork start` and its supervisor).
    Leave,
}

/// The bounds of one call.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub timeout: Duration,
    /// The most of each of stdout and stderr that is kept.
    pub max_output: usize,
    pub leftovers: Leftovers,
}

impl Limits {
    /// `timeout`, the default output budget, strays ended.
    pub(crate) fn new(timeout: Duration) -> Limits {
        Limits {
            timeout,
            max_output: DEFAULT_MAX_OUTPUT,
            leftovers: Leftovers::Kill,
        }
    }

    /// The same, but leaving processes the command started alone.
    pub(crate) fn leaving(self) -> Limits {
        Limits {
            leftovers: Leftovers::Leave,
            ..self
        }
    }
}

/// How a command that was run ended.
pub(crate) struct Output {
    /// Exit status 0 before the deadline.
    pub ok: bool,
    /// The exit status, when the process ended by itself with one (not by a
    /// signal, and not at the deadline).
    pub code: Option<i32>,
    /// The deadline ended it (and killed it).
    pub timed_out: bool,
    /// What it printed, terminal noise removed. At most the output budget:
    /// see `truncated`.
    pub stdout: String,
    pub stderr: String,
    /// A stream went over the output budget: `stdout` or `stderr` holds only
    /// its first bytes. Whoever parses them must not take them for the whole.
    pub truncated: bool,
    /// Failure text: terminal noise removed, last lines of stderr (else stdout).
    pub message: Option<String>,
}

/// Run `command` to completion within `timeout`, in its own process group so
/// a timeout ends everything it started. Standard input is closed; output is
/// collected as it arrives, within the default budget, and processes it left
/// behind are ended. See [`run_bounded`].
pub(crate) fn run(command: Command, timeout: Duration) -> Result<Output> {
    run_bounded(command, None, Limits::new(timeout))
}

/// [`run`] with `input` written to the command's standard input (then closed),
/// from a thread: a command that never reads cannot hold the caller past the
/// deadline. Secrets belong here rather than in arguments or the environment.
#[cfg(test)]
pub(crate) fn run_with_input(
    command: Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    run_bounded(command, input, Limits::new(timeout))
}

/// [`run`] with optional input, under the given [`Limits`].
pub(crate) fn run_bounded(
    mut command: Command,
    input: Option<&[u8]>,
    limits: Limits,
) -> Result<Output> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = input.to_vec();
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = Collector::start(child.stdout.take(), limits.max_output);
    let mut stderr = Collector::start(child.stderr.take(), limits.max_output);
    let deadline = Instant::now() + limits.timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                // Do not leave it running unwatched.
                let _ = crate::hooks::terminate_group(&mut child);
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break crate::hooks::terminate_group(&mut child)?;
        }
        std::thread::sleep(POLL);
    };
    // The process is gone. Its pipes close with it unless something it
    // started holds them: one wait, for both streams, that cannot outlast
    // DRAIN nor (much) the caller's own timeout.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let settle_by = Instant::now() + DRAIN.min(remaining.max(MIN_DRAIN));
    let first = stdout.wait_until(settle_by);
    let second = stderr.wait_until(settle_by);
    // Something still holds a pipe. After a timeout the group was ended
    // already, so a holder that is still there escaped it and cannot be
    // reached from here; otherwise it is what the command left behind.
    if !(first && second) && limits.leftovers == Leftovers::Kill && !timed_out {
        end_group(child.id());
        let reaped_by = Instant::now() + DRAIN;
        stdout.wait_until(reaped_by);
        stderr.wait_until(reaped_by);
    }
    let stdout = stdout.finish();
    let stderr = stderr.finish();
    let truncated = stdout.truncated || stderr.truncated;
    let ok = status.success() && !timed_out;
    let message = (!ok).then(|| {
        let (out, err) = (stdout.for_message(), stderr.for_message());
        let source = if err.trim().is_empty() { &out } else { &err };
        tail(source, 8)
    });
    Ok(Output {
        ok,
        code: if timed_out { None } else { status.code() },
        timed_out,
        stdout: strip_ansi(&String::from_utf8_lossy(&stdout.head)),
        stderr: strip_ansi(&String::from_utf8_lossy(&stderr.head)),
        truncated,
        message: message.filter(|m| !m.is_empty()),
    })
}

/// End the process group `pgid` (the pid of a command run with its own
/// group): TERM, a short grace for it to leave, then KILL. What is in the
/// group at that moment is everything the command started and did not move
/// out of it.
#[cfg(unix)]
fn end_group(pgid: u32) {
    let group = -(pgid as i32);
    let alive = || {
        // SAFETY: signal 0 delivers nothing; it only asks whether the group exists.
        let rc = unsafe { libc::kill(group, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    if !alive() {
        return;
    }
    // SAFETY: plain signal delivery to the process group the command was made leader of.
    unsafe { libc::kill(group, libc::SIGTERM) };
    let until = Instant::now() + STRAY_GRACE;
    while Instant::now() < until && alive() {
        std::thread::sleep(Duration::from_millis(10));
    }
    if alive() {
        // SAFETY: as above.
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn end_group(_pgid: u32) {}

/// What one stream gave: its first bytes up to the budget, the end of the rest.
#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    /// The last bytes of what came after `head` (at most about 2 * [`TAIL`]).
    tail: Vec<u8>,
    truncated: bool,
}

impl Captured {
    fn push(&mut self, chunk: &[u8], max: usize) {
        let room = max.saturating_sub(self.head.len()).min(chunk.len());
        self.head.extend_from_slice(&chunk[..room]);
        if room < chunk.len() {
            self.truncated = true;
            self.tail.extend_from_slice(&chunk[room..]);
            if self.tail.len() > 2 * TAIL {
                let excess = self.tail.len() - TAIL;
                self.tail.drain(..excess);
            }
        }
    }

    /// The text a failure message is made from: all of a stream that fit the
    /// budget, else the end of it, where the complaint is.
    fn for_message(&self) -> String {
        let bytes = if self.truncated {
            &self.tail[self.tail.len().saturating_sub(TAIL)..]
        } else {
            &self.head
        };
        strip_ansi(&String::from_utf8_lossy(bytes))
    }
}

/// One output stream being read on a thread.
struct Collector {
    shared: Arc<Mutex<Captured>>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    /// The thread has seen the end of the stream.
    ended: bool,
}

impl Collector {
    fn start<R: Read + Send + 'static>(stream: Option<R>, max: usize) -> Collector {
        let shared = Arc::new(Mutex::new(Captured::default()));
        let (tx, done) = std::sync::mpsc::channel();
        let thread = stream.map(|mut stream| {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut captured) = shared.lock() {
                                captured.push(&chunk[..n], max);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(());
            })
        });
        let ended = thread.is_none();
        Collector {
            shared,
            done,
            thread,
            ended,
        }
    }

    /// Wait until the stream ended or `at`. Whether it ended.
    fn wait_until(&mut self, at: Instant) -> bool {
        if !self.ended {
            let left = at.saturating_duration_since(Instant::now());
            self.ended = !matches!(self.done.recv_timeout(left), Err(RecvTimeoutError::Timeout));
        }
        self.ended
    }

    /// What was read. A reader that saw the end of its stream is joined (it is
    /// done); one still blocked on a held pipe is detached: it ends when
    /// whatever holds the pipe lets go, and keeps nothing but its buffer.
    fn finish(mut self) -> Captured {
        if self.ended
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
        self.shared
            .lock()
            .map(|mut captured| std::mem::take(&mut *captured))
            .unwrap_or_default()
    }
}

/// The last `lines` non-empty lines.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let from = kept.len().saturating_sub(lines);
    kept[from..].join("\n")
}

/// Drop terminal escape sequences (spinners and cursor movement).
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn terminal_noise_is_removed_from_errors() {
        assert_eq!(
            strip_ansi("\u{1b}[?2026h\u{1b}[1A\u{1b}[2K\u{1b}[?25lfailed to start\u{1b}[?25h"),
            "failed to start"
        );
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(tail("a\n\nb\nc\nd\n", 2), "c\nd");
    }

    #[test]
    fn output_and_failure_are_reported() {
        let ok = run(sh("echo out; echo err >&2"), Duration::from_secs(10)).unwrap();
        assert!(ok.ok && !ok.timed_out && ok.message.is_none());
        assert_eq!((ok.stdout.trim(), ok.stderr.trim()), ("out", "err"));
        let failed = run(sh("echo broken >&2; exit 3"), Duration::from_secs(10)).unwrap();
        assert!(!failed.ok && !failed.timed_out);
        assert_eq!(failed.message.as_deref(), Some("broken"));
    }

    #[test]
    fn input_reaches_the_command_and_the_exit_code_is_reported() {
        let out = run_with_input(
            sh("read first; echo \"got $first\"; exit 4"),
            Some(b"secret-line\nrest\n"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "got secret-line");
        assert!(!out.ok && !out.timed_out);
        assert_eq!(out.code, Some(4));
        // A command that never reads its input is still bounded. (The input
        // fits the pipe: a larger one would fail with a broken pipe once the
        // command is killed, and `devflow exec`'s tests, which run in this
        // process, leave SIGPIPE at its default.)
        let started = Instant::now();
        let idle = run_with_input(
            sh("sleep 30"),
            Some(&vec![b'x'; 16 << 10]),
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(idle.timed_out && idle.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(15));
        // Without input, standard input is closed.
        let closed = run(sh("cat; echo end"), Duration::from_secs(10)).unwrap();
        assert_eq!(closed.stdout.trim(), "end");
        assert_eq!(closed.code, Some(0));
    }

    #[test]
    fn the_deadline_ends_and_reaps_the_command_and_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        // A shell that starts a sleeper and waits for it.
        let script = format!("sleep 60 & echo $! > '{}'; wait", pidfile.display());
        let started = Instant::now();
        let out = run(sh(&script), Duration::from_millis(300)).unwrap();
        assert!(out.timed_out && !out.ok, "timed out and not ok");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The sleeper is gone with its parent (signal 0 only asks).
        let alive = (0..40).any(|_| {
            // SAFETY: signal 0 delivers nothing; it only checks that the process exists.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            if exists {
                std::thread::sleep(Duration::from_millis(50));
            }
            exists
        });
        assert!(!alive, "the process the command started is still running");
    }

    #[test]
    fn a_grandchild_that_keeps_the_pipes_open_does_not_hold_the_caller() {
        let started = Instant::now();
        let out = run(sh("sleep 20 & echo done"), Duration::from_secs(10)).unwrap();
        assert!(out.ok);
        assert_eq!(out.stdout.trim(), "done");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
    }

    /// The pid a script recorded with `echo $! > FILE`.
    fn recorded_pid(file: &std::path::Path) -> i32 {
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Whether the process is still there after waiting up to two seconds for
    /// it to go (signal 0 only asks).
    fn lingers(pid: i32) -> bool {
        (0..40).all(|_| {
            // SAFETY: signal 0 delivers nothing; it only checks that the process exists.
            let exists = unsafe { libc::kill(pid, 0) } == 0;
            if exists {
                std::thread::sleep(Duration::from_millis(50));
            }
            exists
        })
    }

    fn end(pid: i32) {
        // SAFETY: a plain KILL of a process this test started and keeps track of.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[test]
    fn a_stray_holding_the_pipes_is_ended_and_the_call_stays_within_its_budget() {
        // The shell exits at once; its background child keeps both pipes. With a
        // 200 ms budget the call used to return after about a second (two waits
        // of 500 ms, one per stream) and leave the child running.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("stray.pid");
        let script = format!("sleep 60 & echo $! > '{}'; echo done", pidfile.display());
        let started = Instant::now();
        let out = run(sh(&script), Duration::from_millis(200)).unwrap();
        let took = started.elapsed();
        assert!(out.ok && !out.timed_out, "the command itself succeeded");
        assert_eq!(out.stdout.trim(), "done");
        assert!(took < Duration::from_millis(900), "{took:?}");
        let stray = recorded_pid(&pidfile);
        assert!(
            !lingers(stray),
            "the process the command left was not ended"
        );
        end(stray);
    }

    #[test]
    fn leaving_strays_alone_still_bounds_the_wait_and_ends_the_reader() {
        // What `pitchfork start` needs: its supervisor may be in the group.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("stray.pid");
        let script = format!("sleep 60 & echo $! > '{}'; echo done", pidfile.display());
        let started = Instant::now();
        let out = run_bounded(
            sh(&script),
            None,
            Limits::new(Duration::from_secs(10)).leaving(),
        )
        .unwrap();
        let took = started.elapsed();
        assert!(out.ok);
        assert_eq!(out.stdout.trim(), "done");
        assert!(took < Duration::from_secs(2), "{took:?}");
        let stray = recorded_pid(&pidfile);
        assert!(lingers(stray), "a process left alone was ended");
        end(stray);
    }

    #[test]
    fn a_stray_that_ignores_term_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("stray.pid");
        let script = format!(
            "(trap '' TERM; sleep 60) & echo $! > '{}'; echo done",
            pidfile.display()
        );
        let out = run(sh(&script), Duration::from_millis(300)).unwrap();
        assert!(out.ok);
        let stray = recorded_pid(&pidfile);
        assert!(!lingers(stray), "KILL follows TERM");
        end(stray);
    }

    #[test]
    fn output_beyond_the_budget_is_dropped_flagged_and_never_blocks_the_command() {
        let small = Limits {
            max_output: 4096,
            ..Limits::new(Duration::from_secs(20))
        };
        // 200 KB on each stream, far more than a pipe holds: the command only
        // finishes if the excess is still being read.
        let started = Instant::now();
        let out = run_bounded(
            sh("head -c 200000 /dev/zero | tr '\\0' x; head -c 200000 /dev/zero | tr '\\0' y >&2"),
            None,
            small,
        )
        .unwrap();
        assert!(out.ok && !out.timed_out, "{:?}", out.message);
        assert!(out.truncated);
        assert_eq!((out.stdout.len(), out.stderr.len()), (4096, 4096));
        assert!(out.stdout.bytes().all(|b| b == b'x'));
        assert!(started.elapsed() < Duration::from_secs(10));
        // Within the budget nothing is flagged, and the text is whole.
        let fits = run_bounded(sh("head -c 4096 /dev/zero | tr '\\0' x"), None, small).unwrap();
        assert!(!fits.truncated);
        assert_eq!(fits.stdout.len(), 4096);
        // The default is 4 MiB per stream.
        assert_eq!(DEFAULT_MAX_OUTPUT, 4 << 20);
        let big = run(
            sh("head -c 5000000 /dev/zero | tr '\\0' z"),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(big.truncated);
        assert_eq!(big.stdout.len(), DEFAULT_MAX_OUTPUT);
        let normal = run(sh("echo small"), Duration::from_secs(10)).unwrap();
        assert!(!normal.truncated);
    }

    #[test]
    fn the_failure_message_of_a_truncated_stream_comes_from_its_end() {
        let small = Limits {
            max_output: 1024,
            ..Limits::new(Duration::from_secs(20))
        };
        let out = run_bounded(
            sh("head -c 100000 /dev/zero | tr '\\0' x >&2; echo >&2; echo the-real-complaint >&2; exit 3"),
            None,
            small,
        )
        .unwrap();
        assert!(!out.ok && out.truncated);
        assert_eq!(out.code, Some(3));
        let message = out.message.expect("a failure has a message");
        assert!(message.ends_with("the-real-complaint"), "{message}");
        assert!(message.len() < 20_000, "bounded: {}", message.len());
    }

    #[test]
    fn input_and_the_budget_work_together() {
        // Small input only: a large one would fail with a broken pipe once the
        // command is gone, and `devflow exec`'s tests, which run in this
        // process, leave SIGPIPE at its default.
        let out = run_bounded(
            sh("cat; head -c 50000 /dev/zero | tr '\\0' q"),
            Some(b"hello\n"),
            Limits {
                max_output: 100,
                ..Limits::new(Duration::from_secs(20))
            },
        )
        .unwrap();
        assert!(out.ok && out.truncated);
        assert!(out.stdout.starts_with("hello\nqqq"));
        assert_eq!(out.stdout.len(), 100);
    }
}
