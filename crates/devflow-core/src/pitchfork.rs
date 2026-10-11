//! Thin optional adapter over the `pitchfork` CLI (https://pitchfork.jdx.dev).
//! devflow does not supervise processes; the GUI lists and starts/stops the
//! daemons pitchfork knows for a workspace.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize)]
pub struct Daemon {
    /// Pitchfork's own identifier (namespace/name), used verbatim for start/stop.
    pub id: String,
    pub status: String,
    pub pid: Option<u32>,
    pub dir: Option<PathBuf>,
}

/// The `pitchfork` executable on `PATH` (a mise shim counts).
pub fn binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("pitchfork"))
        .find(|candidate| is_executable(candidate))
}

pub fn available() -> bool {
    binary().is_some()
}

/// Daemons pitchfork reports for the project containing `workspace_path`
/// (`pitchfork list --project --json` run there): the ones configured in the
/// workspace's `pitchfork.toml` plus any that were started from it. When
/// pitchfork reports a directory, daemons outside the workspace are dropped.
pub fn daemons(workspace_path: &Path) -> Result<Vec<Daemon>> {
    let text = run(workspace_path, &["list", "--project", "--json"])?;
    let root = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.to_path_buf());
    Ok(parse_list(&text)?
        .into_iter()
        .filter(|d| {
            d.dir.as_ref().is_none_or(|dir| {
                dir.canonicalize()
                    .unwrap_or_else(|_| dir.clone())
                    .starts_with(&root)
            })
        })
        .collect())
}

/// Start daemon `id` (as listed by [`daemons`]). Blocks until pitchfork
/// considers it ready.
pub fn start(workspace_path: &Path, id: &str) -> Result<()> {
    run(workspace_path, &["start", checked(id)?]).map(|_| ())
}

pub fn stop(workspace_path: &Path, id: &str) -> Result<()> {
    run(workspace_path, &["stop", checked(id)?]).map(|_| ())
}

fn checked(id: &str) -> Result<&str> {
    ensure!(
        !id.is_empty() && !id.starts_with('-'),
        "invalid daemon id '{id}'"
    );
    Ok(id)
}

fn run(workspace_path: &Path, args: &[&str]) -> Result<String> {
    let binary = binary().context("pitchfork is not installed (not found on PATH)")?;
    let output = Command::new(binary)
        .args(args)
        .current_dir(workspace_path)
        .stdin(Stdio::null())
        .output()
        .context("cannot run pitchfork")?;
    if !output.status.success() {
        let mut detail = strip_ansi(&String::from_utf8_lossy(&output.stderr));
        if detail.trim().is_empty() {
            detail = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        }
        bail!("`pitchfork {}` failed: {}", args.join(" "), detail.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `pitchfork list --json`: an array of objects with at least `id`.
/// Unknown fields are ignored; entries without an id are skipped.
fn parse_list(text: &str) -> Result<Vec<Daemon>> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).context("pitchfork printed invalid JSON")?;
    let items = value
        .as_array()
        .context("pitchfork list --json did not print an array")?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            Some(Daemon {
                id,
                status: item
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                pid: item
                    .get("pid")
                    .and_then(|p| p.as_u64())
                    .and_then(|p| u32::try_from(p).ok()),
                dir: ["dir", "directory", "cwd"]
                    .iter()
                    .find_map(|key| item.get(*key)?.as_str())
                    .map(PathBuf::from),
            })
        })
        .collect())
}

/// Drop terminal escape sequences (pitchfork draws spinners).
fn strip_ansi(text: &str) -> String {
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

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real output of `pitchfork list --json` (2.29.0).
    const LISTING: &str = r#"[
      {"id":"proj/sleeper","namespace":"proj","name":"sleeper","pid":54357,"status":"running",
       "oneshot":false,"disabled":false,"available":false,"proxy_url":null,"url":null,"error":null,"port":[]},
      {"id":"proj/web","namespace":"proj","name":"web","pid":null,"status":"available",
       "oneshot":false,"disabled":false,"available":true,"proxy_url":"https://web.proj.localhost","url":null,"error":null,"port":[3000]}
    ]"#;

    #[test]
    fn parses_pitchfork_listings() {
        let daemons = parse_list(LISTING).unwrap();
        assert_eq!(daemons.len(), 2);
        assert_eq!(daemons[0].id, "proj/sleeper");
        assert_eq!(daemons[0].status, "running");
        assert_eq!(daemons[0].pid, Some(54357));
        assert_eq!(daemons[0].dir, None);
        assert_eq!(daemons[1].pid, None);
        assert_eq!(daemons[1].status, "available");
    }

    #[test]
    fn parsing_is_defensive() {
        assert!(parse_list("[]").unwrap().is_empty());
        assert!(parse_list("not json").is_err());
        assert!(parse_list("{}").is_err());
        let odd = parse_list(r#"[{"status":"running"},{"id":"a/b","pid":-1,"dir":"/w/x","status":7},"junk",{"id":"c/d","cwd":"/w/y"}]"#).unwrap();
        assert_eq!(odd.len(), 2);
        assert_eq!(odd[0].id, "a/b");
        assert_eq!((odd[0].pid, odd[0].status.as_str()), (None, "unknown"));
        assert_eq!(odd[0].dir.as_deref(), Some(Path::new("/w/x")));
        assert_eq!(odd[1].dir.as_deref(), Some(Path::new("/w/y")));
    }

    #[test]
    fn terminal_noise_is_removed_from_errors() {
        assert_eq!(
            strip_ansi("\u{1b}[?2026h\u{1b}[1A\u{1b}[2K\u{1b}[?25lfailed to start\u{1b}[?25h"),
            "failed to start"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn option_like_ids_are_refused() {
        assert!(checked("--all").is_err());
        assert!(checked("").is_err());
        assert_eq!(checked("proj/web").unwrap(), "proj/web");
    }
}
