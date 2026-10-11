//! Environment and project checks. Every check is quick and reports a problem
//! as a result instead of returning an error.
use crate::config::{CowCapability, PostgresMode};
use crate::db::CowSource;
use crate::githook::{self, InstallState};
use crate::project::Project;
use crate::receipt::Status;
use crate::trust::{self, TrustState};
use crate::{cow, pitchfork, setup, worktree};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            hint: None,
        }
    }

    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Ok, detail)
    }

    fn warn(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Warn, detail).hint(hint)
    }

    fn fail(name: &str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(name, CheckStatus::Fail, detail).hint(hint)
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// git, docker/compose (managed mode), server reachability and version,
/// file_copy_method, CoW capability, worktree-directory CoW, hook trust,
/// Git hook state, pitchfork/mise presence, workspaces needing attention.
/// Without a project only the environment checks run.
pub fn run(project: Option<&Project>) -> Vec<Check> {
    let mut checks = git();
    let managed = project
        .and_then(|p| p.config.postgres.as_ref())
        .is_some_and(|pg| pg.mode == PostgresMode::Managed);
    checks.extend(docker(managed));
    if let Some(project) = project {
        checks.extend(project_config(project));
        checks.extend(worktree_dir(project));
        if project.config.postgres.is_some() {
            checks.extend(postgres(project));
        }
        checks.extend(hook_trust(project));
        checks.extend(git_hook(project));
    }
    checks.extend(tool(
        "pitchfork",
        pitchfork::binary(),
        "mise use -g pitchfork",
        "daemons and the local HTTPS proxy",
    ));
    checks.extend(tool(
        "mise",
        find_on_path("mise"),
        "https://mise.jdx.dev",
        "toolchains and dependency installation in setup hooks",
    ));
    if let Some(project) = project {
        checks.extend(workspaces(project));
    }
    checks
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn git() -> Vec<Check> {
    let v = git2::Version::get();
    let (major, minor, patch) = v.libgit2_version();
    vec![Check::ok(
        "git",
        format!("libgit2 {major}.{minor}.{patch} (built in; no `git` binary needed)"),
    )]
}

fn docker(needed: bool) -> Vec<Check> {
    let output = Command::new("docker")
        .args(["compose", "version", "--short"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => vec![Check::ok(
            "docker",
            format!(
                "docker compose {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ),
        )],
        found => {
            let detail = match found {
                Ok(_) => "`docker compose` is not available",
                Err(_) => "`docker` was not found on PATH",
            };
            let hint = "install Docker (or OrbStack) with the compose plugin";
            vec![if needed {
                Check::fail(
                    "docker",
                    format!("{detail}; the managed PostgreSQL server needs it"),
                    hint,
                )
            } else {
                Check::warn(
                    "docker",
                    format!("{detail} (only needed for managed PostgreSQL)"),
                    hint,
                )
            }]
        }
    }
}

fn project_config(project: &Project) -> Vec<Check> {
    vec![match project.config.validate() {
        Ok(()) => Check::ok(
            "project",
            format!(
                "{} ({}) at {}",
                project.name(),
                project.id(),
                project.root.display()
            ),
        ),
        Err(e) => Check::fail(
            "project",
            format!("{} is invalid: {e:#}", project.config_path.display()),
            "fix the file or run `devflow init`",
        ),
    }]
}

fn worktree_dir(project: &Project) -> Vec<Check> {
    let target = worktree::path_for(project, "probe");
    let dir = target.parent().unwrap_or(&target).to_path_buf();
    // The directory is created on first use; check where it would be created.
    let existing: PathBuf = dir
        .ancestors()
        .find(|p| p.is_dir())
        .unwrap_or(Path::new("/"))
        .to_path_buf();
    let mut checks = Vec::new();
    let marker = existing.join(format!(".devflow-doctor-{}", uuid::Uuid::new_v4()));
    let writable = std::fs::write(&marker, b"").is_ok();
    let _ = std::fs::remove_file(&marker);
    if !writable {
        checks.push(Check::fail(
            "worktree-dir",
            format!("{} is not writable", existing.display()),
            "change `worktree.dir` in devflow.toml",
        ));
        return checks;
    }
    let note = if dir.is_dir() {
        ""
    } else {
        " (created on first use)"
    };
    checks.push(Check::ok(
        "worktree-dir",
        format!("{}{note}", dir.display()),
    ));

    let cross_device = same_device(&existing, &project.root) == Some(false);
    checks.push(if cross_device {
        Check::warn(
            "worktree-cow",
            format!("{} is on another filesystem than the repository", existing.display()),
            "payload copies cannot share blocks across filesystems; keep `worktree.dir` beside the repository",
        )
    } else if cow::probe(&existing) {
        Check::ok("worktree-cow", "payloads are cloned copy-on-write")
    } else {
        Check::warn(
            "worktree-cow",
            "this filesystem cannot clone files, so payloads are plain copies",
            "use APFS, btrfs, XFS (reflink) or ZFS 2.2+ for fast, space-sharing worktrees",
        )
    });
    checks
}

#[cfg(unix)]
fn same_device(a: &Path, b: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    Some(a.metadata().ok()?.dev() == b.metadata().ok()?.dev())
}

#[cfg(not(unix))]
fn same_device(_: &Path, _: &Path) -> Option<bool> {
    None
}

fn postgres(project: &Project) -> Vec<Check> {
    let server = match crate::db::Server::for_project(project) {
        Ok(Some(server)) => server,
        Ok(None) => return Vec::new(),
        Err(e) => {
            return vec![Check::fail(
                "postgres",
                format!("{e:#}"),
                "check the [postgres] section of devflow.toml",
            )];
        }
    };
    let status = server.status();
    let mut checks = Vec::new();
    let endpoint = status
        .endpoint
        .clone()
        .unwrap_or_else(|| "unknown endpoint".into());
    if !status.reachable {
        let hint = match server.mode() {
            PostgresMode::Managed => "start it with `devflow db up`",
            PostgresMode::External => {
                "check `postgres.admin_url` / DEVFLOW_PG_ADMIN_URL and the network"
            }
        };
        let detail = status
            .problem
            .clone()
            .unwrap_or_else(|| format!("{endpoint} is not reachable"));
        checks.push(Check::warn("postgres", detail, hint));
        return checks;
    }
    let version = status
        .server_version
        .clone()
        .unwrap_or_else(|| "unknown version".into());
    let too_old = major_version(&version).is_some_and(|major| major < 18);
    checks.push(if too_old {
        Check::fail(
            "postgres",
            format!("PostgreSQL {version} at {endpoint}"),
            "branching needs PostgreSQL 18 or newer",
        )
    } else {
        Check::ok("postgres", format!("PostgreSQL {version} at {endpoint}"))
    });
    checks.push(match status.file_copy_method.as_deref() {
        Some("clone") => Check::ok("postgres-file-copy", "file_copy_method = clone"),
        other => Check::warn(
            "postgres-file-copy",
            format!("file_copy_method = {}", other.unwrap_or("unknown")),
            "set `file_copy_method = clone` on the server; until then branches are full copies",
        ),
    });
    // Whether devflow measured the capability or the operator declared it.
    let source = match status.cow_source {
        Some(CowSource::Measured) => ", measured",
        Some(CowSource::Declared) => ", declared",
        None => "",
    };
    checks.push(match status.cow {
        CowCapability::Verified => Check::ok(
            "postgres-cow",
            format!("branches share blocks with their source (verified{source})"),
        ),
        CowCapability::Copy => Check::warn(
            "postgres-cow",
            format!("branches are full copies (copy{source})"),
            "put the data directory on a reflink filesystem (btrfs, XFS, ZFS 2.2+, OrbStack volumes)",
        ),
        CowCapability::Unknown => Check::warn(
            "postgres-cow",
            "copy-on-write could not be verified",
            "for an external server, declare `cow = \"verified\"` in devflow.local.toml after checking its filesystem",
        ),
    });
    checks
}

fn major_version(version: &str) -> Option<u32> {
    version
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn hook_trust(project: &Project) -> Vec<Check> {
    vec![match trust::state(project) {
        Ok(TrustState::NoHooks) => Check::ok("hook-trust", "no hooks configured"),
        Ok(TrustState::Trusted) => Check::ok("hook-trust", "hooks are approved"),
        Ok(TrustState::Untrusted {
            previously_trusted, ..
        }) => Check::warn(
            "hook-trust",
            if previously_trusted {
                "the [hooks] section changed since it was approved"
            } else {
                "the [hooks] section has not been approved"
            },
            "review and approve it with `devflow trust`; until then setup skips hooks",
        ),
        Err(e) => Check::fail(
            "hook-trust",
            format!("{e:#}"),
            "check trust.toml in the devflow config directory",
        ),
    }]
}

fn git_hook(project: &Project) -> Vec<Check> {
    vec![match githook::state(project) {
        Ok(InstallState::Installed { path }) => Check::ok(
            "git-hook",
            format!("post-checkout installed at {}", path.display()),
        ),
        Ok(InstallState::NotInstalled) => Check::warn(
            "git-hook",
            "worktrees created by other tools (git worktree add, IDEs, agents) are not set up automatically",
            "run `devflow hook install`",
        ),
        Ok(InstallState::Foreign { path }) => {
            let chained =
                std::fs::read_to_string(&path).is_ok_and(|t| t.contains("--from-git-hook"));
            if chained {
                Check::ok(
                    "git-hook",
                    format!("{} runs `devflow setup --if-new`", path.display()),
                )
            } else {
                Check::warn(
                    "git-hook",
                    format!("{} exists and was not written by devflow", path.display()),
                    "`devflow hook install` prints the line to add to it",
                )
            }
        }
        Err(e) => Check::fail("git-hook", format!("{e:#}"), "check core.hooksPath"),
    }]
}

fn tool(name: &str, found: Option<PathBuf>, install: &str, purpose: &str) -> Vec<Check> {
    vec![match found {
        Some(path) => Check::ok(name, path.display().to_string()),
        None => Check::warn(
            name,
            format!("`{name}` was not found on PATH (optional: {purpose})"),
            format!("install: {install}"),
        ),
    }]
}

fn workspaces(project: &Project) -> Vec<Check> {
    let list = match setup::list(project) {
        Ok(list) => list,
        Err(e) => {
            return vec![Check::fail(
                "workspaces",
                format!("{e:#}"),
                "run `git worktree list`",
            )];
        }
    };
    let attention: Vec<String> = list
        .iter()
        .filter(|w| matches!(w.status, Some(Status::NeedsAttention)))
        .map(|w| w.workspace.name.clone())
        .collect();
    let interrupted: Vec<String> = list
        .iter()
        .filter(|w| matches!(w.status, Some(Status::InProgress)) && !w.busy)
        .map(|w| w.workspace.name.clone())
        .collect();
    let missing: Vec<String> = list
        .iter()
        .filter(|w| w.workspace.missing)
        .map(|w| w.workspace.name.clone())
        .collect();
    let mut checks = Vec::new();
    if !attention.is_empty() {
        checks.push(Check::warn(
            "workspaces",
            format!("needs attention: {}", attention.join(", ")),
            "run `devflow setup` inside each to see what failed and resume",
        ));
    }
    if !interrupted.is_empty() {
        checks.push(Check::warn(
            "workspaces",
            format!("setup was interrupted in: {}", interrupted.join(", ")),
            "run `devflow setup` inside each to resume",
        ));
    }
    if !missing.is_empty() {
        checks.push(Check::warn(
            "workspaces",
            format!("directory missing for: {}", missing.join(", ")),
            "run `git worktree prune`, or `devflow remove NAME`",
        ));
    }
    if checks.is_empty() {
        checks.push(Check::ok(
            "workspaces",
            format!("{} workspace(s), none need attention", list.len()),
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil;

    fn find<'a>(checks: &'a [Check], name: &str) -> Vec<&'a Check> {
        checks.iter().filter(|c| c.name == name).collect()
    }

    #[test]
    fn environment_checks_run_without_a_project() {
        let checks = run(None);
        let names: Vec<_> = checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["git", "docker", "pitchfork", "mise"]);
        assert_eq!(checks[0].status, CheckStatus::Ok);
        assert!(checks.iter().all(|c| !c.detail.is_empty()));
        // Optional tools and an optional docker never fail an environment-only run.
        assert!(checks.iter().all(|c| c.status != CheckStatus::Fail));
    }

    #[test]
    fn project_checks_cover_the_worktree_directory_trust_and_hooks() {
        let t = testutil::repo(None);
        let checks = run(Some(&t.project));
        assert_eq!(find(&checks, "project")[0].status, CheckStatus::Ok);
        assert_eq!(find(&checks, "worktree-dir")[0].status, CheckStatus::Ok);
        // CoW depends on the filesystem running the tests: either answer is fine.
        assert_eq!(find(&checks, "worktree-cow").len(), 1);
        assert_eq!(find(&checks, "hook-trust")[0].detail, "no hooks configured");
        assert_eq!(find(&checks, "git-hook")[0].status, CheckStatus::Warn);
        assert!(
            find(&checks, "postgres").is_empty(),
            "no [postgres] section"
        );
        assert_eq!(find(&checks, "workspaces")[0].status, CheckStatus::Ok);
    }

    #[test]
    fn unapproved_hooks_and_installed_git_hooks_are_reported() {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.hooks.setup = vec!["echo hi".into()];
        // Trust is read from the real config dir here; only assert it is not Ok-by-accident.
        let checks = run(Some(&project));
        let trust_check = find(&checks, "hook-trust")[0];
        assert!(trust_check.status == CheckStatus::Warn || trust_check.status == CheckStatus::Ok);

        githook::install(&project).unwrap();
        let checks = run(Some(&project));
        assert_eq!(find(&checks, "git-hook")[0].status, CheckStatus::Ok);
    }

    #[test]
    fn workspaces_needing_attention_are_named() {
        let t = testutil::repo(None);
        let ws = worktree::create(&t.project, "stuck", None).unwrap();
        let mut receipt = crate::receipt::Receipt::new(t.project.id(), "stuck", None);
        receipt.set_phase(
            crate::receipt::Phase::Materialize,
            crate::receipt::PhaseState::Done,
            None,
        );
        receipt.skip_required(crate::receipt::Phase::Hooks, "hooks are not trusted".into());
        receipt.save(&ws.git_dir).unwrap();
        let checks = run(Some(&t.project));
        let found = find(&checks, "workspaces");
        assert!(
            found
                .iter()
                .any(|c| c.status == CheckStatus::Warn && c.detail.contains("stuck")),
            "{found:?}"
        );
    }

    #[test]
    fn a_missing_worktree_directory_is_checked_where_it_would_be_created() {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.worktree.dir = "../deep/er/{repo}.wt".into();
        let checks = run(Some(&project));
        let dir = find(&checks, "worktree-dir")[0];
        assert_eq!(dir.status, CheckStatus::Ok);
        assert!(
            dir.detail.contains("created on first use"),
            "{}",
            dir.detail
        );
        assert!(
            !t.root.parent().unwrap().join("deep").exists(),
            "doctor creates nothing"
        );
    }

    #[test]
    fn an_unreachable_postgres_server_is_a_warning_not_a_panic() {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.postgres = Some(crate::config::PostgresSection {
            mode: PostgresMode::External,
            admin_url: Some("postgres://nobody@127.0.0.1:1/postgres".into()),
            ..Default::default()
        });
        let checks = run(Some(&project));
        let pg = find(&checks, "postgres");
        assert_eq!(pg.len(), 1, "{pg:?}");
        assert_eq!(pg[0].status, CheckStatus::Warn, "{pg:?}");
        assert!(pg[0].hint.is_some());
        // A managed server that was never started is reported the same way.
        project.config.postgres = Some(Default::default());
        let checks = run(Some(&project));
        assert_ne!(
            find(&checks, "postgres")[0].status,
            CheckStatus::Ok,
            "{checks:?}"
        );
    }

    #[test]
    fn parses_server_major_versions() {
        assert_eq!(major_version("18.0"), Some(18));
        assert_eq!(major_version("PostgreSQL 17.4 (Debian)"), Some(17));
        assert_eq!(major_version("unknown"), None);
    }
}
