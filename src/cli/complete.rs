//! Dynamic shell completion: workspace names for the commands that take one.
//!
//! A completion runs every time somebody presses Tab, so it reads local Git and
//! the workspaces' receipts and nothing else: no server, no Docker, no
//! pitchfork. A directory that is not a devflow project completes nothing.
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use devflow_core::Project;
use devflow_core::worktree;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The completer for an argument that names a workspace.
pub fn workspace() -> ArgValueCompleter {
    ArgValueCompleter::new(workspace_names)
}

fn workspace_names(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    // `devflow -C ../other switch <Tab>` completes the names of that project.
    let words: Vec<OsString> = std::env::args_os().collect();
    let dir = effective_dir(&words, &cwd);
    let Ok(project) = Project::discover(&dir) else {
        return Vec::new();
    };
    let Ok(names) = worktree::names(&project) else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter(|named| named.name.starts_with(current))
        .map(|named| {
            let help = if named.missing {
                format!("{} (missing)", named.path.display())
            } else {
                named.path.display().to_string()
            };
            CompletionCandidate::new(named.name).help(Some(help.into()))
        })
        .collect()
}

/// The directory the command being completed runs in: its last `-C DIR`
/// (`-C DIR`, `-CDIR`, `-C=DIR`) among the words the shell handed over after
/// the `--` that separates them from devflow's own, relative to `cwd`; else
/// `cwd`. The word being typed counts too, so `-C ../o<Tab>` completes against
/// the project it already names only once it is a directory.
fn effective_dir(args: &[OsString], cwd: &Path) -> PathBuf {
    let words = args
        .iter()
        .skip_while(|word| *word != "--")
        .skip(1)
        // (The first word is the program the user typed.)
        .skip(1);
    let mut dir: Option<PathBuf> = None;
    let mut words = words;
    while let Some(word) = words.next() {
        let Some(text) = word.to_str() else { continue };
        if text == "--" {
            break;
        }
        if text == "-C" {
            if let Some(value) = words.next() {
                dir = Some(PathBuf::from(value));
            }
        } else if let Some(value) = text.strip_prefix("-C") {
            let value = value.strip_prefix('=').unwrap_or(value);
            dir = Some(PathBuf::from(value));
        }
    }
    match dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(dir) => cwd.join(dir),
        None => cwd.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_completed_commands_dash_c_is_found() {
        let cwd = Path::new("/work/here");
        let dir = |list: &[&str]| effective_dir(&words(list), cwd);
        assert_eq!(dir(&["devflow", "--", "devflow", "switch", ""]), cwd);
        assert_eq!(
            dir(&["devflow", "--", "devflow", "-C", "/abs", "switch", "f"]),
            Path::new("/abs")
        );
        assert_eq!(
            dir(&["devflow", "--", "devflow", "-C", "../other", "path", ""]),
            Path::new("/work/here/../other")
        );
        assert_eq!(
            dir(&["devflow", "--", "devflow", "switch", "-C/abs", ""]),
            Path::new("/abs")
        );
        assert_eq!(
            dir(&["devflow", "--", "devflow", "-C=/abs", "path", ""]),
            Path::new("/abs")
        );
        // The last one wins, as it does for clap; nothing after a `--` counts.
        assert_eq!(
            dir(&[
                "devflow", "--", "devflow", "-C", "/a", "-C", "/b", "path", "", "--", "-C", "/c"
            ]),
            Path::new("/b")
        );
        // Not completing: no `--`, no directory.
        assert_eq!(dir(&["devflow", "-C", "/abs"]), cwd);
    }
}
