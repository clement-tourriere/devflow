//! `examples/agent-task.sh` against a fake `devflow` on PATH: the script must
//! stay the parent of the command it runs, so the workspace is removed after a
//! success and after a failure, `KEEP=1` keeps it, and INT/TERM reach the
//! command. Needs a POSIX shell and `jq` (skipped without `jq`).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The script's world: a fake worktree, a fake `devflow` that logs its
/// arguments, and a directory to run in.
struct Run {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Run {
    fn new() -> Option<Run> {
        if Command::new("jq").arg("--version").output().is_err() {
            eprintln!("skipped: jq is not installed");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["bin", "main", "worktree"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let fake = root.join("bin/devflow");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
echo "$*" >> "$FAKE_LOG"
if [ "$1" = exec ]; then
  shift 2
  exec "$@"
fi
case "$*" in
  *" remove "*)
    # Is a process the command started still alive when the cleanup runs?
    if [ -n "$FAKE_PIDFILE" ] && [ -f "$FAKE_PIDFILE" ] && kill -0 "$(cat "$FAKE_PIDFILE")" 2>/dev/null; then
      echo alive >> "$FAKE_ALIVE"
    fi
    ;;
  *"switch -c"*)
    printf '{"status":"ready","workspace":{"path":"%s"},"receipt":{"services":{}}}\n' "$FAKE_WORKTREE"
    ;;
esac
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Some(Run { _tmp: tmp, root })
    }

    fn log(&self) -> PathBuf {
        self.root.join("devflow.log")
    }

    fn command(&self, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("bash");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/agent-task.sh"))
            .args(args)
            .current_dir(self.root.join("main"))
            .env("PATH", path)
            .env("FAKE_LOG", self.log())
            .env("FAKE_WORKTREE", self.root.join("worktree"))
            .env("FAKE_PIDFILE", self.pidfile())
            .env("FAKE_ALIVE", self.root.join("alive"))
            .env_remove("AGENT_TASK_GRACE")
            .env_remove("KEEP")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    /// Where a command writes the pid of the descendant it starts.
    fn pidfile(&self) -> PathBuf {
        self.root.join("descendant.pid")
    }

    /// The descendant's pid, once the command has written it.
    fn descendant(&self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(self.pidfile())
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "no descendant pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether a process with this pid is running.
    fn running(pid: i32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The cleanup ran while a process of the command was still alive.
    fn cleaned_up_over_a_live_process(&self) -> bool {
        self.root.join("alive").exists()
    }

    /// The fake devflow's calls, one per line.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn removed(&self) -> bool {
        self.calls()
            .iter()
            .any(|call| call == "--json --non-interactive remove agent/t --force")
    }
}

#[test]
fn a_successful_command_is_followed_by_the_cleanup() {
    let Some(run) = Run::new() else { return };
    let out = run.command(&["t", "sh", "-c", "exit 0"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(run.removed(), "{:?}", run.calls());
    // Created first, then the command through `devflow exec`, then removed.
    let calls = run.calls();
    assert!(calls[0].contains("switch -c agent/t"), "{calls:?}");
    assert!(calls[1].starts_with("exec -- sh -c"), "{calls:?}");
    assert!(
        calls.last().unwrap().contains("remove agent/t"),
        "{calls:?}"
    );
}

#[test]
fn a_failing_command_keeps_its_exit_status_and_is_followed_by_the_cleanup() {
    let Some(run) = Run::new() else { return };
    let out = run.command(&["t", "sh", "-c", "exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(run.removed(), "{:?}", run.calls());
}

#[test]
fn keep_leaves_the_workspace_after_success_and_failure() {
    for code in ["0", "4"] {
        let Some(run) = Run::new() else { return };
        let out = run
            .command(&["t", "sh", "-c", &format!("exit {code}")])
            .env("KEEP", "1")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(code.parse().unwrap()));
        assert!(!run.removed(), "{:?}", run.calls());
        assert_eq!(run.calls().len(), 2, "{:?}", run.calls());
    }
}

#[test]
fn the_command_runs_in_the_worktree() {
    let Some(run) = Run::new() else { return };
    let marker = run.root.join("cwd");
    let out = run
        .command(&["t", "sh", "-c", &format!("pwd -P > {}", marker.display())])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(marker).unwrap().trim(),
        run.root.join("worktree").to_str().unwrap()
    );
}

/// Start the script with a command that announces itself, wait for that,
/// send `signal` to the script and return its exit status.
fn signalled(signal: &str) -> (Run, Option<i32>, String) {
    let run = Run::new().expect("jq is checked by the caller");
    let ready = run.root.join("ready");
    // The command exits 9 when it receives the signal; a signal that never
    // arrives leaves it sleeping until the test gives up.
    let body = format!(
        "trap 'echo got-{signal}; exit 9' {signal}; : > {}; i=0; while [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done; exit 1",
        ready.display()
    );
    let child = run
        .command(&["t", "sh", "-c", &body])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let status = Command::new("kill")
        .args(["-s", signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let out = child.wait_with_output().unwrap();
    (
        run,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn term_reaches_the_command_and_the_cleanup_still_runs() {
    if Run::new().is_none() {
        return;
    }
    let (run, code, stdout) = signalled("TERM");
    assert_eq!(code, Some(9), "{stdout}");
    assert!(stdout.contains("got-TERM"), "{stdout}");
    assert!(run.removed(), "{:?}", run.calls());
}

#[test]
fn int_reaches_the_command_and_the_cleanup_still_runs() {
    if Run::new().is_none() {
        return;
    }
    let (run, code, stdout) = signalled("INT");
    assert_eq!(code, Some(9), "{stdout}");
    assert!(stdout.contains("got-INT"), "{stdout}");
    assert!(run.removed(), "{:?}", run.calls());
}

/// A command that starts a sleeping descendant, records its pid, and then
/// `ending`.
fn with_descendant(run: &Run, ending: &str) -> String {
    format!(
        "sleep 30 & echo $! > {}; : > {}; {ending}",
        run.pidfile().display(),
        run.root.join("ready").display()
    )
}

#[test]
fn a_descendant_left_behind_by_a_normal_exit_is_stopped_before_the_cleanup() {
    let Some(run) = Run::new() else { return };
    let body = with_descendant(&run, "exit 5");
    let started = Instant::now();
    let out = run.command(&["t", "sh", "-c", &body]).output().unwrap();
    // The command's own status is kept; the task did not wait for the sleep.
    assert_eq!(out.status.code(), Some(5));
    assert!(started.elapsed() < Duration::from_secs(20));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("left processes in its group"), "{stderr}");
    assert!(
        !Run::running(run.descendant()),
        "the sleep outlived the task"
    );
    assert!(run.removed(), "{:?}", run.calls());
    assert!(
        !run.cleaned_up_over_a_live_process(),
        "the workspace was removed while the command's process ran"
    );
}

#[test]
fn a_descendant_that_ignores_term_is_killed_after_the_grace() {
    let Some(run) = Run::new() else { return };
    // The shell ignores TERM and so does the sleep it starts.
    let body = format!("trap '' TERM; {}", with_descendant(&run, "exit 0"));
    let started = Instant::now();
    let out = run
        .command(&["t", "sh", "-c", &body])
        .env("AGENT_TASK_GRACE", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("killing it"), "{stderr}");
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the grace was not observed"
    );
    assert!(!Run::running(run.descendant()));
    assert!(!run.cleaned_up_over_a_live_process());
    assert!(run.removed(), "{:?}", run.calls());
}

/// Signal the script while its command waits on a sleeping descendant.
fn signalled_over_a_descendant(signal: &str) -> (Run, Option<i32>) {
    let run = Run::new().expect("jq is checked by the caller");
    let body = with_descendant(&run, "wait");
    let child = run.command(&["t", "sh", "-c", &body]).spawn().unwrap();
    let ready = run.root.join("ready");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let descendant = run.descendant();
    assert!(Run::running(descendant));
    let status = Command::new("kill")
        .args(["-s", signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let out = child.wait_with_output().unwrap();
    (run, out.status.code())
}

#[test]
fn term_reaches_the_whole_group_and_the_cleanup_waits_for_it() {
    if Run::new().is_none() {
        return;
    }
    let (run, code) = signalled_over_a_descendant("TERM");
    // The shell was killed by the forwarded TERM (143), not left waiting.
    assert_eq!(code, Some(143));
    assert!(
        !Run::running(run.descendant()),
        "the sleep outlived the task"
    );
    assert!(!run.cleaned_up_over_a_live_process());
    assert!(run.removed(), "{:?}", run.calls());
}

#[test]
fn int_reaches_the_whole_group_and_the_cleanup_waits_for_it() {
    if Run::new().is_none() {
        return;
    }
    // A non-interactive shell starts its background sleep with SIGINT ignored,
    // so the sleep survives the INT its leader dies of: the script then stops
    // it (TERM) before the cleanup.
    let (run, code) = signalled_over_a_descendant("INT");
    // The shell died of a signal (the status says which, and a shell reports
    // INT it did not handle in more than one way).
    assert!(code.is_some_and(|c| c > 128), "{code:?}");
    assert!(
        !Run::running(run.descendant()),
        "the sleep outlived the task"
    );
    assert!(!run.cleaned_up_over_a_live_process());
    assert!(run.removed(), "{:?}", run.calls());
}

// ---- cancellation that the command ignores, and a group that cannot be stopped ----------

/// Start the script with a command that ignores TERM (and so does what it
/// starts), wait until it runs, send it TERM and return how it ended and how
/// long that took after the signal.
fn term_ignored(grace: &str) -> (Run, Option<i32>, String, Duration) {
    let run = Run::new().expect("jq is checked by the caller");
    let ready = run.root.join("ready");
    // Loops for 30 s; only KILL ends it earlier.
    let body = format!(
        "trap '' TERM; : > {}; i=0; while [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; exit 1",
        ready.display()
    );
    let child = run
        .command(&["t", "sh", "-c", &body])
        .env("AGENT_TASK_GRACE", grace)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "the command never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    let signalled = Instant::now();
    let status = Command::new("kill")
        .args(["-s", "TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let out = child.wait_with_output().unwrap();
    (
        run,
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        signalled.elapsed(),
    )
}

#[test]
fn a_command_that_ignores_term_is_killed_as_soon_as_a_zero_grace_is_over() {
    if Run::new().is_none() {
        return;
    }
    // The command's leader stays alive: the grace is enforced while it runs,
    // not only after it exits. KILL ends it, its real status (128 + 9) is
    // the script's, and the cleanup runs after that.
    let (run, code, stderr, took) = term_ignored("0");
    assert_eq!(code, Some(137), "{stderr}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    assert!(stderr.contains("killing its process group"), "{stderr}");
    assert!(run.removed(), "{:?}", run.calls());
    assert!(!run.cleaned_up_over_a_live_process());
}

#[test]
fn a_command_that_ignores_term_gets_the_whole_grace_after_the_signal() {
    if Run::new().is_none() {
        return;
    }
    let (run, code, stderr, took) = term_ignored("2");
    assert_eq!(code, Some(137), "{stderr}");
    assert!(
        took >= Duration::from_secs(2),
        "killed before the grace was over: {took:?}"
    );
    assert!(took < Duration::from_secs(15), "{took:?}");
    assert!(run.removed(), "{:?}", run.calls());
    assert!(!run.cleaned_up_over_a_live_process());
}

#[test]
fn a_grace_with_a_leading_zero_is_a_decimal_number_and_the_cleanup_still_waits() {
    let Some(run) = Run::new() else { return };
    // A descendant that takes a second to die once it is asked to: with "08"
    // read as an (invalid) octal number the script reached the cleanup at once,
    // over a live process, and returned the command's 7.
    let slow = format!(
        "sh -c \"trap 'sleep 1; exit 0' TERM; echo \\$\\$ > {}; while :; do sleep 0.2; done\" &",
        run.pidfile().display()
    );
    let body = format!(
        "{slow} while [ ! -s {} ]; do sleep 0.05; done; exit 7",
        run.pidfile().display()
    );
    let out = run
        .command(&["t", "sh", "-c", &body])
        .env("AGENT_TASK_GRACE", "08")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(7), "{stderr}");
    assert!(!stderr.contains("value too great"), "{stderr}");
    assert!(
        !Run::running(run.descendant()),
        "the descendant outlived the task"
    );
    assert!(
        !run.cleaned_up_over_a_live_process(),
        "the workspace was removed while the command's process ran"
    );
    assert!(run.removed(), "{:?}", run.calls());
}

#[test]
fn a_grace_that_is_not_a_bounded_decimal_number_is_refused_before_anything_is_created() {
    for grace in ["abc", "-1", "1.5", "3601", "99999999999999999999", " 5"] {
        let Some(run) = Run::new() else { return };
        let out = run
            .command(&["t", "sh", "-c", "exit 0"])
            .env("AGENT_TASK_GRACE", grace)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "{grace:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("AGENT_TASK_GRACE"), "{grace:?}: {stderr}");
        assert!(run.calls().is_empty(), "{grace:?}: {:?}", run.calls());
    }
    // And the extremes that are valid.
    for grace in ["0", "00", "010", "3600"] {
        let Some(run) = Run::new() else { return };
        let out = run
            .command(&["t", "sh", "-c", "exit 0"])
            .env("AGENT_TASK_GRACE", grace)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{grace:?}");
        assert!(run.removed(), "{grace:?}: {:?}", run.calls());
    }
}

/// Stops the process whose pid the command wrote, when the test ends.
struct Reaper(PathBuf);

impl Drop for Reaper {
    fn drop(&mut self) {
        if let Ok(text) = std::fs::read_to_string(&self.0) {
            let _ = Command::new("kill").args(["-9", text.trim()]).status();
        }
    }
}

/// A `kill` function exported to bash: KILL does nothing, so a process that
/// ignores TERM is one that "survives KILL" (a process in uninterruptible
/// sleep does). Every other signal, and `kill -0`, is the real thing.
const KILL_THAT_DOES_NOTHING: (&str, &str) = (
    "BASH_FUNC_kill%%",
    "() {  case \"$*\" in \"-s KILL\"*) return 0 ;; esac\n builtin kill \"$@\"\n}",
);

/// Whether this bash imports functions from the environment.
fn imports_exported_functions() -> bool {
    Command::new("bash")
        .args(["-c", "type -t kill"])
        .env(KILL_THAT_DOES_NOTHING.0, KILL_THAT_DOES_NOTHING.1)
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim() == "function")
}

#[test]
fn a_group_that_survives_kill_keeps_the_workspace_and_exits_70() {
    let Some(run) = Run::new() else { return };
    if !imports_exported_functions() {
        eprintln!("skipped: this bash does not import exported functions");
        return;
    }
    let _reaper = Reaper(run.pidfile());
    // The command exits 5 and leaves a sleeper that ignores TERM; the faked
    // KILL does not reach it.
    // (Its output goes nowhere: it must not hold the script's pipes open
    // after the script has exited.)
    let body = format!(
        "trap '' TERM; sleep 30 >/dev/null 2>&1 & echo $! > {}; exit 5",
        run.pidfile().display()
    );
    let started = Instant::now();
    let out = run
        .command(&["t", "sh", "-c", &body])
        .env("AGENT_TASK_GRACE", "1")
        .env(KILL_THAT_DOES_NOTHING.0, KILL_THAT_DOES_NOTHING.1)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Not the command's status, and not "not ready" (2).
    assert_eq!(out.status.code(), Some(70), "{stderr}");
    let pid = run.descendant();
    assert!(Run::running(pid), "the sleeper is the point of the test");
    // The message names the workspace, the processes and the status.
    assert!(stderr.contains("agent/t"), "{stderr}");
    assert!(stderr.contains("NOT removed"), "{stderr}");
    assert!(stderr.contains(&pid.to_string()), "{stderr}");
    assert!(stderr.contains("status was 5"), "{stderr}");
    // The workspace is kept: the cleanup never ran `remove`.
    assert!(!run.removed(), "{:?}", run.calls());
    // Grace (1 s) plus the confirmation after KILL (5 s), bounded.
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(20),
        "{took:?}"
    );
}
