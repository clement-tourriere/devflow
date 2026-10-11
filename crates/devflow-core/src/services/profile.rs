//! The Valkey / Redis storage profile: what devflow configures, how a running
//! server is quiesced for a clone, and how a new one is checked.
//!
//! devflow generates every container's configuration, so it controls
//! persistence. The profile is: AOF off, RDB on with a save policy, data in
//! `/data/dump.rdb`, and a `requirepass` of the container's own. Anything else
//! (an ACL file, an append-only source) is not a configuration devflow can
//! clone, and is refused rather than copied wrongly.
//!
//! Consistency: a clone is the dataset at the fork of a `BGSAVE` (the
//! reported capture time). `BGSAVE` writes a temporary file and renames it,
//! so a file that exists is always a whole dump; the source is never paused
//! (pausing an RDB-only server measured an empty clone) and keeps serving
//! writes while the file is copied.
//!
//! The exception is a source that has nothing to save: when `INFO
//! persistence` says no change was made since the last save, no save is
//! running and the last one succeeded, the file already on disk is that
//! dataset and is cloned as it is ([`last_save`]). Asking for a `BGSAVE`
//! anyway would write a new file while the previous one stays shared with
//! every earlier clone's volume, which costs a full dataset of blocks per
//! clone. Such a clone reports its capture as the last save, with the time
//! the server gives (`rdb_last_save_time`).
//!
//! Identity: an RDB file carries data only, no credentials, so the clone's
//! `--requirepass` (new and its own) is the whole identity fix-up, and it is
//! applied before the server listens, because it is on the command line the
//! container starts with.
use super::resp::Conn;
use crate::config::Engine;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The port the server listens on inside the container.
pub const CONTAINER_PORT: u16 = 6379;
/// The data directory inside the container.
pub const DATA_DIR: &str = "/data";
/// The dump file in the data directory.
pub const DUMP_FILE: &str = "dump.rdb";
/// RDB save points: the servers' own defaults (an hour with one change,
/// five minutes with a hundred, a minute with ten thousand). A graceful stop
/// saves too, so `devflow service down` keeps the data.
pub const SAVE_POLICY: &str = "3600 1 300 100 60 10000";

/// The command line of a container of `engine`. `password` is the container's
/// own `requirepass`.
pub fn command(engine: Engine, password: &str) -> Vec<String> {
    let binary = match engine {
        Engine::Valkey => "valkey-server",
        _ => "redis-server",
    };
    [
        binary,
        "--appendonly",
        "no",
        "--save",
        SAVE_POLICY,
        "--dir",
        DATA_DIR,
        "--dbfilename",
        DUMP_FILE,
        "--requirepass",
        password,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// `redis://:<password>@127.0.0.1:<port>/0`, the password percent-encoded.
pub fn url(password: &str, host: &str, port: u16) -> String {
    use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
    format!(
        "redis://:{}@{host}:{port}/0",
        utf8_percent_encode(password, NON_ALPHANUMERIC)
    )
}

/// A new random password for a container.
pub fn new_password() -> String {
    use rand::distr::{Alphanumeric, SampleString};
    Alphanumeric.sample_string(&mut rand::rng(), 32)
}

/// Authenticate and `PING` until the server answers, for up to `timeout`.
/// A server still loading its dataset answers with an error, which is
/// retried; a wrong password is not.
pub fn health(port: u16, password: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let Err(e) = try_health(port, password) else {
            return Ok(());
        };
        let text = format!("{e:#}");
        if text.contains("WRONGPASS") || text.contains("authentication failed") {
            bail!("the server rejected its own password: {text}");
        }
        if Instant::now() >= deadline {
            bail!("the server on 127.0.0.1:{port} did not become healthy: {text}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn try_health(port: u16, password: &str) -> Result<()> {
    let mut conn = Conn::connect(port, Duration::from_secs(3))?;
    conn.auth(password)?;
    conn.ping()?;
    let persistence = conn.info("persistence")?;
    anyhow::ensure!(
        persistence.get("loading").map(String::as_str) != Some("1"),
        "the server is still loading its dataset"
    );
    Ok(())
}

/// What the server reported before the save was requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    /// `rdb_saves`: saves completed since the server started.
    pub saves: u64,
    pub run_id: String,
}

/// How a requested background save is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Waiting,
    Complete,
    Failed(String),
}

/// Refuse a server this profile cannot clone. It must report the counters
/// the completion check relies on (`rdb_saves` exists from Redis 7.0 and
/// Valkey 7.2) and run with AOF off.
pub fn check_capability(
    persistence: &BTreeMap<String, String>,
    server: &BTreeMap<String, String>,
) -> Result<Baseline> {
    for key in [
        "rdb_bgsave_in_progress",
        "rdb_last_bgsave_status",
        "rdb_saves",
    ] {
        anyhow::ensure!(
            persistence.contains_key(key),
            "the server does not report `{key}` in INFO persistence (Redis before 7.0?): devflow \
             cannot tell when a save completed, so it will not clone this server"
        );
    }
    anyhow::ensure!(
        persistence.get("aof_enabled").map(String::as_str) != Some("1"),
        "the server runs with the append-only file on: devflow clones only servers with AOF off \
         (its own profile), because dump.rdb alone would miss the append-only log"
    );
    let run_id = server
        .get("run_id")
        .context("the server does not report its run_id")?
        .clone();
    let saves = persistence["rdb_saves"]
        .parse()
        .context("`rdb_saves` is not a number")?;
    Ok(Baseline { saves, run_id })
}

/// Decide, from `INFO`, whether the save requested after `baseline` is
/// complete. `LASTSAVE` is not enough: it has one-second resolution and
/// cannot tell this save from the one before it. A save is complete when none
/// is running, the last one succeeded, the save counter moved past the
/// baseline and the server is the same process (`run_id`).
pub fn judge(
    baseline: &Baseline,
    persistence: &BTreeMap<String, String>,
    run_id: &str,
) -> Progress {
    if run_id != baseline.run_id {
        return Progress::Failed(
            "the server restarted while it was being saved (its run_id changed)".into(),
        );
    }
    if persistence
        .get("rdb_bgsave_in_progress")
        .map(String::as_str)
        == Some("1")
    {
        return Progress::Waiting;
    }
    match persistence
        .get("rdb_last_bgsave_status")
        .map(String::as_str)
    {
        Some("ok") => {}
        other => {
            return Progress::Failed(format!(
                "the background save failed (rdb_last_bgsave_status: {})",
                other.unwrap_or("missing")
            ));
        }
    }
    match persistence
        .get("rdb_saves")
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(saves) if saves > baseline.saves => Progress::Complete,
        _ => Progress::Failed(
            "the background save ended without being counted (rdb_saves did not advance)".into(),
        ),
    }
}

/// The result of a completed save, or of finding that none was needed.
#[derive(Debug, Clone)]
pub struct Quiesced {
    /// When the server forked for the save: the instant the data was captured.
    /// For a reused save, the server's own `rdb_last_save_time`.
    pub captured_at: DateTime<Utc>,
    /// `rdb_saves` right after the save, to notice a later one.
    pub saves: u64,
    /// The capture is the last save the server had already made, not one
    /// devflow asked for.
    pub last_save: bool,
}

/// Decide, from `INFO persistence`, whether the file the server already wrote
/// is the live dataset, so a clone can copy it without asking for a save.
///
/// Why that is sound: the server writes an RDB to a temporary file and renames
/// it over `dump.rdb`, so a file that exists is always a complete one, and a
/// reflink of it sees one whole version. `rdb_changes_since_last_save` counts
/// the writes since the last save started, so 0 means the live dataset is what
/// that save wrote. A write that races this check is in the same position as
/// one racing a `BGSAVE`: whether the clone has it is a matter of the instant,
/// and the reported capture time says which instant. A key that expired since
/// is dropped on load, as for any RDB. Every other condition (a save running,
/// the last one failed, a server still loading, missing counters) keeps the
/// ordinary `BGSAVE` path.
///
/// `None` when the file cannot be reused. This cannot see whether `dump.rdb`
/// exists: the caller checks that.
pub fn reusable_last_save(
    persistence: &BTreeMap<String, String>,
    server: &BTreeMap<String, String>,
) -> Option<Quiesced> {
    let baseline = check_capability(persistence, server).ok()?;
    let text = |name: &str| persistence.get(name).map(String::as_str);
    if text("rdb_changes_since_last_save") != Some("0")
        || text("rdb_bgsave_in_progress") != Some("0")
        || text("rdb_last_bgsave_status") != Some("ok")
        || text("loading") == Some("1")
    {
        return None;
    }
    let saved = text("rdb_last_save_time")?.parse::<i64>().ok()?;
    Some(Quiesced {
        captured_at: DateTime::from_timestamp(saved, 0)?,
        saves: baseline.saves,
        last_save: true,
    })
}

/// Ask the running server on `port` whether its last save can be cloned as it
/// is ([`reusable_last_save`]).
pub fn last_save(port: u16, password: &str) -> Result<Option<Quiesced>> {
    let mut conn = Conn::connect(port, Duration::from_secs(5))?;
    conn.auth(password)?;
    let persistence = conn.info("persistence")?;
    let server = conn.info("server")?;
    Ok(reusable_last_save(&persistence, &server))
}

/// Make the running server on `port` write a complete `dump.rdb` and wait
/// for it, without pausing it. Returns when the file is whole.
pub fn quiesce(port: u16, password: &str, timeout: Duration) -> Result<Quiesced> {
    let deadline = Instant::now() + timeout;
    let mut conn = Conn::connect(port, Duration::from_secs(5))?;
    conn.auth(password)?;
    let persistence = conn.info("persistence")?;
    let server = conn.info("server")?;
    check_capability(&persistence, &server)?;
    // Our own save must be the next one: wait out a save already running.
    let mut attempts = 0;
    let (baseline, captured_at) = loop {
        wait_idle(&mut conn, deadline)?;
        let persistence = conn.info("persistence")?;
        let server = conn.info("server")?;
        let baseline = check_capability(&persistence, &server)?;
        match conn.command(&["BGSAVE"])? {
            super::resp::Value::Error(message)
                if message.contains("already in progress")
                    || message.contains("Another child process") =>
            {
                // A save or rewrite won the race; wait for it and ask again.
            }
            super::resp::Value::Error(message) => bail!("BGSAVE was refused: {message}"),
            _ => {
                // The server forked before it answered: this is the capture.
                break (baseline, Utc::now());
            }
        }
        attempts += 1;
        anyhow::ensure!(attempts < 20, "another save kept running; giving up");
    };
    loop {
        let persistence = conn.info("persistence")?;
        let server = conn.info("server")?;
        let run_id = server.get("run_id").cloned().unwrap_or_default();
        match judge(&baseline, &persistence, &run_id) {
            Progress::Complete => {
                let saves = persistence["rdb_saves"]
                    .parse()
                    .unwrap_or(baseline.saves + 1);
                return Ok(Quiesced {
                    captured_at,
                    saves,
                    last_save: false,
                });
            }
            Progress::Failed(why) => bail!("{why}"),
            Progress::Waiting => {}
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the background save did not finish in time"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_idle(conn: &mut Conn, deadline: Instant) -> Result<()> {
    loop {
        let persistence = conn.info("persistence")?;
        if persistence
            .get("rdb_bgsave_in_progress")
            .map(String::as_str)
            != Some("1")
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "a background save was still running at the deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Where the source keeps its dump, as the server says; the clone copies
/// `/data/dump.rdb`, so any other place is refused.
pub fn check_layout(port: u16, password: &str) -> Result<()> {
    let mut conn = Conn::connect(port, Duration::from_secs(5))?;
    conn.auth(password)?;
    let dir = conn.config_get("dir")?;
    let file = conn.config_get("dbfilename")?;
    anyhow::ensure!(
        dir.as_deref() == Some(DATA_DIR) && file.as_deref() == Some(DUMP_FILE),
        "the server keeps its dump in {}/{}, not {DATA_DIR}/{DUMP_FILE}: it was not started with \
         devflow's profile",
        dir.unwrap_or_default(),
        file.unwrap_or_default()
    );
    Ok(())
}

/// Stop the server on `port` without writing its dataset (for the throwaway
/// staging container: saving would rewrite the file a reflink shares).
pub fn shutdown_nosave(port: u16, password: &str) {
    if let Ok(mut conn) = Conn::connect(port, Duration::from_secs(3))
        && conn.auth(password).is_ok()
    {
        conn.shutdown_nosave();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn persistence(in_progress: &str, status: &str, saves: &str) -> BTreeMap<String, String> {
        map(&[
            ("rdb_bgsave_in_progress", in_progress),
            ("rdb_last_bgsave_status", status),
            ("rdb_saves", saves),
            ("aof_enabled", "0"),
        ])
    }

    #[test]
    fn the_profile_has_aof_off_rdb_on_and_the_containers_own_password() {
        let valkey = command(Engine::Valkey, "pw1");
        assert_eq!(valkey[0], "valkey-server");
        let joined = valkey.join(" ");
        assert!(joined.contains("--appendonly no"), "{joined}");
        assert!(joined.contains(&format!("--save {SAVE_POLICY}")));
        assert!(joined.contains("--dir /data"));
        assert!(joined.contains("--dbfilename dump.rdb"));
        assert!(joined.ends_with("--requirepass pw1"));
        assert_eq!(command(Engine::Redis, "x")[0], "redis-server");
        // The save policy is one argument, as the servers parse it.
        let save = valkey.iter().position(|a| a == "--save").unwrap();
        assert_eq!(valkey[save + 1], SAVE_POLICY);
    }

    #[test]
    fn urls_carry_the_password_encoded() {
        // kingfisher:ignore
        assert_eq!(
            url("abc123", "127.0.0.1", 23456),
            "redis://:abc123@127.0.0.1:23456/0" // kingfisher:ignore
        );
        assert_eq!(url("a/b@c", "h", 1), "redis://:a%2Fb%40c@h:1/0"); // kingfisher:ignore
        assert_eq!(new_password().len(), 32);
        assert_ne!(new_password(), new_password());
    }

    #[test]
    fn servers_that_cannot_prove_a_save_or_use_aof_are_refused() {
        let server = map(&[("run_id", "abc")]);
        let baseline = check_capability(&persistence("0", "ok", "4"), &server).unwrap();
        assert_eq!(
            baseline,
            Baseline {
                saves: 4,
                run_id: "abc".into()
            }
        );
        // Before Redis 7.0 there is no save counter.
        let mut old = persistence("0", "ok", "4");
        old.remove("rdb_saves");
        let error = check_capability(&old, &server).unwrap_err().to_string();
        assert!(
            error.contains("rdb_saves") && error.contains("will not clone"),
            "{error}"
        );
        let mut aof = persistence("0", "ok", "4");
        aof.insert("aof_enabled".into(), "1".into());
        assert!(
            check_capability(&aof, &server)
                .unwrap_err()
                .to_string()
                .contains("AOF off")
        );
        assert!(check_capability(&persistence("0", "ok", "4"), &map(&[])).is_err());
    }

    #[test]
    fn an_unchanged_source_reuses_its_last_save_and_anything_else_saves() {
        let server = map(&[("run_id", "abc")]);
        let mut idle = persistence("0", "ok", "4");
        idle.insert("rdb_changes_since_last_save".into(), "0".into());
        idle.insert("rdb_last_save_time".into(), "1700000000".into());
        let reused = reusable_last_save(&idle, &server).expect("nothing changed");
        assert!(reused.last_save);
        assert_eq!(reused.captured_at.timestamp(), 1_700_000_000);
        assert_eq!(reused.saves, 4);
        let without = |name: &str| {
            let mut m = idle.clone();
            m.remove(name);
            m
        };
        let with = |name: &str, value: &str| {
            let mut m = idle.clone();
            m.insert(name.into(), value.into());
            m
        };
        // A write since the last save, a save running, a failed save, a server
        // still loading, and missing counters all keep the BGSAVE path.
        for case in [
            with("rdb_changes_since_last_save", "1"),
            with("rdb_bgsave_in_progress", "1"),
            with("rdb_last_bgsave_status", "err"),
            with("loading", "1"),
            without("rdb_changes_since_last_save"),
            without("rdb_last_save_time"),
            without("rdb_saves"),
            with("aof_enabled", "1"),
            with("rdb_last_save_time", "soon"),
        ] {
            assert!(reusable_last_save(&case, &server).is_none(), "{case:?}");
        }
        assert!(reusable_last_save(&idle, &map(&[])).is_none());
    }

    #[test]
    fn a_save_is_complete_only_when_counted_ok_and_by_the_same_process() {
        let base = Baseline {
            saves: 4,
            run_id: "abc".into(),
        };
        // Running, or not yet counted while still running.
        assert_eq!(
            judge(&base, &persistence("1", "ok", "4"), "abc"),
            Progress::Waiting
        );
        assert_eq!(
            judge(&base, &persistence("1", "ok", "5"), "abc"),
            Progress::Waiting
        );
        // Done and counted.
        assert_eq!(
            judge(&base, &persistence("0", "ok", "5"), "abc"),
            Progress::Complete
        );
        // LASTSAVE-style evidence is not enough: nothing moved.
        assert!(matches!(
            judge(&base, &persistence("0", "ok", "4"), "abc"),
            Progress::Failed(why) if why.contains("rdb_saves did not advance")
        ));
        // A failed save, even with a counter that moved.
        assert!(matches!(
            judge(&base, &persistence("0", "err", "5"), "abc"),
            Progress::Failed(why) if why.contains("rdb_last_bgsave_status: err")
        ));
        // A restart resets the counter and the run id.
        assert!(matches!(
            judge(&base, &persistence("0", "ok", "1"), "xyz"),
            Progress::Failed(why) if why.contains("run_id changed")
        ));
        assert!(matches!(
            judge(&base, &map(&[("rdb_bgsave_in_progress", "0")]), "abc"),
            Progress::Failed(_)
        ));
    }
}
