//! The external tools devflow drives, and the oldest release of each it
//! supports.
//!
//! The pins live here and nowhere else: [`MIN_PITCHFORK`], [`MIN_MISE`] and
//! [`MIN_GIT`]. A pin is the newest release of the features devflow calls
//! unconditionally, never a guess about what "recent" means, so each is derived
//! from the code that needs it and raised only when the code starts to need
//! more:
//!
//! | tool | pin | what it is the release of |
//! | --- | --- | --- |
//! | pitchfork | 2.20.0 | `supervisor status --json` (2.20.0); `list --json` and `proxy status --json` (2.14.0); `start` and `stop` by `namespace/name`, since daemon ids carry a namespace (2.0.0) |
//! | mise | 2026.4.18 | `mise exec --no-deps` (2026.4.18), which keeps the hook check from preparing dependencies, and `--fresh-env` (2026.1.6), which keeps it from answering out of a cache |
//! | Git | 2.19.0 | `git worktree add --quiet` (2.19.0); only the native checkout (`[worktree] checkout = "git"`) runs Git |
//!
//! A release older than the pin, or one whose version cannot be read, is a
//! typed [`ToolProblem`]: a sentence that names the tool, the release and the
//! pin (never a command line), and the [`Remedy`] values that fix it. `doctor`
//! reports it, the pitchfork gate refuses to start or stop with it, the mise
//! gate refuses to run hooks with it and the native checkout refuses to start.
//! Nothing here installs or upgrades anything.
use crate::remedy::Remedy;
use crate::subprocess;
use serde::{Serialize, Serializer};
use std::fmt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The oldest pitchfork devflow supports.
pub const MIN_PITCHFORK: Version = Version::new(2, 20, 0);
/// The oldest mise devflow supports (its releases are dated: `2026.4.18`).
pub const MIN_MISE: Version = Version::new(2026, 4, 18);
/// The oldest Git the native checkout supports.
pub const MIN_GIT: Version = Version::new(2, 19, 0);

/// How long `--version` may take (through a mise shim it can be slow the first
/// time).
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// A release number: `2.30.1`, or mise's dated `2026.10.7`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// The first word of `text` that starts with a number (`pitchfork 2.29.0`,
    /// `git version 2.50.1 (Apple Git-155)`, `2026.10.7 macos-arm64 (…)`,
    /// `v2.30.1-beta.1`) read as `major.minor[.patch]`; what follows a number
    /// in a component (`1-beta`) and further components are ignored. A word
    /// without a minor version is not a release number.
    pub fn parse(text: &str) -> Option<Version> {
        let word = text
            .split_whitespace()
            .map(|word| word.strip_prefix('v').unwrap_or(word))
            .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
        let mut parts = word.split('.').map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u32>()
                .ok()
        });
        let major = parts.next().flatten()?;
        let minor = parts.next().flatten()?;
        let patch = parts.next().flatten().unwrap_or(0);
        Some(Version::new(major, minor, patch))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A tool with a pinned minimum release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Pitchfork,
    Mise,
    Git,
}

impl Tool {
    pub const fn name(self) -> &'static str {
        match self {
            Tool::Pitchfork => "pitchfork",
            Tool::Mise => "mise",
            Tool::Git => "Git",
        }
    }

    /// The oldest release devflow supports.
    pub const fn minimum(self) -> Version {
        match self {
            Tool::Pitchfork => MIN_PITCHFORK,
            Tool::Mise => MIN_MISE,
            Tool::Git => MIN_GIT,
        }
    }

    /// What the pin is the release of, for the sentence that explains it.
    const fn needs(self) -> &'static str {
        match self {
            Tool::Pitchfork => "the supervisor status it reads",
            Tool::Mise => "the `exec` options its hook check passes",
            Tool::Git => "the `worktree add` options the native checkout passes",
        }
    }

    /// The page that says how to install or upgrade the tool.
    pub const fn upgrade_page(self) -> &'static str {
        match self {
            Tool::Pitchfork => "https://github.com/jdx/pitchfork/releases",
            Tool::Mise => "https://mise.jdx.dev/installing-mise.html",
            Tool::Git => "https://git-scm.com/downloads",
        }
    }
}

/// Why an installed tool cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Reason {
    /// An older release than the pin.
    TooOld { found: Version },
    /// `--version` printed no release number devflow can read.
    Unreadable,
}

/// A tool that runs but is not one devflow supports: typed, so a frontend
/// draws the fix instead of parsing a sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolProblem {
    pub tool: Tool,
    #[serde(flatten)]
    pub reason: Reason,
    /// The pin the release was judged against.
    pub minimum: Version,
}

impl ToolProblem {
    pub fn new(tool: Tool, reason: Reason) -> Self {
        Self {
            tool,
            reason,
            minimum: tool.minimum(),
        }
    }

    /// What is wrong, in a sentence for any frontend: the tool, the release,
    /// the pin and what the pin is for. No command line.
    pub fn message(&self) -> String {
        let (name, minimum) = (self.tool.name(), self.minimum);
        let needs = self.tool.needs();
        match &self.reason {
            Reason::TooOld { found } => format!(
                "{name} {found} is older than {minimum}, the oldest release devflow supports \
                 ({needs})"
            ),
            Reason::Unreadable => format!(
                "the release of {name} cannot be read from what it prints, and devflow needs \
                 {minimum} or later ({needs})"
            ),
        }
    }

    /// What can be done, most useful first. pitchfork can be upgraded through
    /// mise, which devflow offers and never does unasked; every tool has a page
    /// that says how to install or upgrade it.
    pub fn remedies(&self) -> Vec<Remedy> {
        let mut remedies = Vec::new();
        if self.tool == Tool::Pitchfork {
            remedies.push(Remedy::InstallPitchfork);
        }
        remedies.push(Remedy::OpenGuide {
            url: self.tool.upgrade_page().to_string(),
        });
        remedies
    }
}

impl fmt::Display for ToolProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// Judge what `tool --version` printed against the pin: the release, or why
/// it cannot be used.
pub fn judge(tool: Tool, printed: &str) -> Result<Version, ToolProblem> {
    match Version::parse(printed) {
        Some(found) if found >= tool.minimum() => Ok(found),
        Some(found) => Err(ToolProblem::new(tool, Reason::TooOld { found })),
        None => Err(ToolProblem::new(tool, Reason::Unreadable)),
    }
}

/// What asking an executable for its version found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// A supported release.
    Supported(Version),
    /// It runs, and it is not a release devflow supports.
    Unsupported(ToolProblem),
    /// It does not run (or not in time); the reason names no value.
    DoesNotRun(String),
}

/// Run `program --version` (bounded) and judge it against the pin of `tool`.
pub fn probe(tool: Tool, program: &Path) -> Probe {
    let mut command = Command::new(program);
    command.arg("--version").env("NO_COLOR", "1");
    match subprocess::run(command, VERSION_TIMEOUT) {
        Ok(out) if out.ok => match judge(tool, &out.stdout) {
            Ok(version) => Probe::Supported(version),
            Err(problem) => Probe::Unsupported(problem),
        },
        Ok(out) if out.timed_out => Probe::DoesNotRun(format!(
            "`{} --version` did not finish within {}s",
            program.display(),
            VERSION_TIMEOUT.as_secs()
        )),
        Ok(out) => Probe::DoesNotRun(
            out.message
                .map(|message| crate::redact::text(&message))
                .unwrap_or_else(|| format!("`{} --version` failed", program.display())),
        ),
        Err(error) => Probe::DoesNotRun(format!("cannot run {}: {error}", program.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Option<String> {
        Version::parse(text).map(|v| v.to_string())
    }

    #[test]
    fn release_numbers_are_read_from_the_first_numeric_word() {
        assert_eq!(version("pitchfork 2.29.0\n").as_deref(), Some("2.29.0"));
        assert_eq!(version("2.30.1").as_deref(), Some("2.30.1"));
        assert_eq!(version("v2.30.1-beta.1").as_deref(), Some("2.30.1"));
        assert_eq!(
            version("git version 2.50.1 (Apple Git-155)").as_deref(),
            Some("2.50.1")
        );
        assert_eq!(
            version("git version 2.39.3.windows.1").as_deref(),
            Some("2.39.3")
        );
        // mise dates its releases; the date after the platform is not a version.
        assert_eq!(
            version("2026.10.7 macos-arm64 (2026-10-09)").as_deref(),
            Some("2026.10.7")
        );
        assert_eq!(version("2.30").as_deref(), Some("2.30.0"));
        // Not release numbers.
        for text in [
            "",
            "pitchfork",
            "2",
            "unknown",
            "mise ERROR no version",
            "v",
            "x.y.z",
        ] {
            assert_eq!(version(text), None, "{text:?}");
        }
    }

    #[test]
    fn releases_are_compared_as_numbers_not_as_text() {
        let v = |text: &str| Version::parse(text).unwrap();
        assert!(v("2.100.0") > v("2.20.0"));
        assert!(v("2026.10.7") > v("2026.4.18"));
        assert!(v("2026.4.18") > v("2026.4.9"));
        assert!(v("3.0.0") > v("2.99.99"));
    }

    #[test]
    fn a_release_is_judged_against_the_pin_of_its_tool() {
        assert_eq!(
            judge(Tool::Pitchfork, "pitchfork 2.20.0"),
            Ok(Version::new(2, 20, 0))
        );
        assert!(judge(Tool::Pitchfork, "pitchfork 2.30.1").is_ok());
        assert!(judge(Tool::Pitchfork, "3.0.0").is_ok());
        let old = judge(Tool::Pitchfork, "pitchfork 2.19.9").unwrap_err();
        assert_eq!(
            old.reason,
            Reason::TooOld {
                found: Version::new(2, 19, 9)
            }
        );
        // The same number means different things to each tool.
        assert!(judge(Tool::Git, "git version 2.19.0").is_ok());
        assert!(judge(Tool::Git, "git version 2.18.9").is_err());
        assert!(judge(Tool::Mise, "2026.4.18 linux-x64 (2026-04-19)").is_ok());
        assert!(judge(Tool::Mise, "2026.4.17 linux-x64 (2026-04-18)").is_err());
        assert!(judge(Tool::Mise, "2.30.1").is_err());
    }

    #[test]
    fn a_version_that_cannot_be_read_is_not_a_supported_one() {
        for tool in [Tool::Pitchfork, Tool::Mise, Tool::Git] {
            let problem = judge(tool, "garbled").unwrap_err();
            assert_eq!(problem.reason, Reason::Unreadable);
            assert_eq!(problem.minimum, tool.minimum());
        }
    }

    #[test]
    fn a_problem_names_the_tool_the_release_and_the_pin_without_a_command() {
        let old = judge(Tool::Mise, "2026.3.1").unwrap_err();
        let message = old.message();
        for needle in ["mise 2026.3.1", "older than 2026.4.18", "oldest release"] {
            assert!(message.contains(needle), "{message}");
        }
        assert!(
            !message.contains("mise use") && !message.contains("`devflow"),
            "{message}"
        );
        let unreadable = judge(Tool::Pitchfork, "garbled").unwrap_err().message();
        assert!(
            unreadable.contains("cannot be read") && unreadable.contains("2.20.0 or later"),
            "{unreadable}"
        );
    }

    #[test]
    fn the_remedies_open_the_upgrade_page_and_pitchfork_can_be_upgraded_through_mise() {
        let url = |remedies: &[Remedy]| {
            remedies
                .iter()
                .find_map(|r| match r {
                    Remedy::OpenGuide { url } => Some(url.clone()),
                    _ => None,
                })
                .expect("a page to open")
        };
        let pitchfork = judge(Tool::Pitchfork, "1.6.0").unwrap_err().remedies();
        assert_eq!(pitchfork[0], Remedy::InstallPitchfork);
        assert_eq!(url(&pitchfork), "https://github.com/jdx/pitchfork/releases");
        let mise = judge(Tool::Mise, "2025.1.0").unwrap_err().remedies();
        assert_eq!(mise.len(), 1, "{mise:?}");
        assert_eq!(url(&mise), "https://mise.jdx.dev/installing-mise.html");
        assert_eq!(
            url(&judge(Tool::Git, "git version 2.1.0")
                .unwrap_err()
                .remedies()),
            "https://git-scm.com/downloads"
        );
    }

    #[test]
    fn a_problem_serializes_as_data_a_frontend_can_read() {
        let old = judge(Tool::Pitchfork, "2.14.0").unwrap_err();
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({
                "tool": "pitchfork",
                "reason": "too_old",
                "found": "2.14.0",
                "minimum": "2.20.0"
            })
        );
        let unreadable = judge(Tool::Mise, "").unwrap_err();
        assert_eq!(
            serde_json::to_value(&unreadable).unwrap(),
            serde_json::json!({"tool": "mise", "reason": "unreadable", "minimum": "2026.4.18"})
        );
    }

    #[cfg(unix)]
    #[test]
    fn probing_runs_the_executable_and_judges_what_it_prints() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        // mise prints its version on stdout and notices on stderr.
        let current = tool(
            "current",
            "echo 'mise WARN a newer mise is available' >&2; echo '2026.10.7 macos-arm64 (2026-10-09)'",
        );
        assert_eq!(
            probe(Tool::Mise, &current),
            Probe::Supported(Version::new(2026, 10, 7))
        );
        let old = tool("old", "echo '2026.3.1 macos-arm64 (2026-03-02)'");
        assert!(matches!(
            probe(Tool::Mise, &old),
            Probe::Unsupported(ToolProblem {
                reason: Reason::TooOld { .. },
                ..
            })
        ));
        let garbled = tool("garbled", "echo 'something else'");
        assert!(matches!(
            probe(Tool::Mise, &garbled),
            Probe::Unsupported(ToolProblem {
                reason: Reason::Unreadable,
                ..
            })
        ));
        // A shim with no version configured exits 1.
        let shim = tool(
            "shim",
            "echo 'mise ERROR No version is set for shim' >&2; exit 1",
        );
        assert!(matches!(probe(Tool::Mise, &shim), Probe::DoesNotRun(_)));
        assert!(matches!(
            probe(Tool::Mise, &dir.path().join("absent")),
            Probe::DoesNotRun(_)
        ));
    }
}
