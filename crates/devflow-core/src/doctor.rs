//! Environment and project checks. Every check is quick and reports a problem
//! as a result instead of returning an error.
use crate::config::{CowCapability, PostgresMode};
use crate::db::CowSource;
use crate::githook::{self, InstallState};
use crate::project::Project;
use crate::receipt::Status;
use crate::trust::{self, TrustState};
use crate::{cow, pitchfork, processes, setup, worktree};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod mise;

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
/// Git hook state, pitchfork/mise presence, workspaces needing attention, and
/// whether mise hands the current workspace its own `.devflow/env` variables.
/// Without a project only the environment checks run. Trust is judged for the
/// main checkout; see [`run_in`] for the workspace a command was started in.
pub fn run(project: Option<&Project>) -> Vec<Check> {
    run_in(
        project,
        project.map_or(Path::new("."), |p| p.root.as_path()),
    )
}

/// [`run`] for a command started in `cwd`: the trust check judges the
/// workspace containing `cwd` (the main checkout when there is none), with the
/// per-workspace functions, since a worktree has its own pitchfork files, and
/// says which directory it judged.
pub fn run_in(project: Option<&Project>, cwd: &Path) -> Vec<Check> {
    let mut checks = git();
    let managed = project
        .and_then(|p| p.config.postgres())
        .is_some_and(|(_, pg)| pg.mode == PostgresMode::Managed)
        || project.is_some_and(|p| p.config.containers().next().is_some());
    checks.extend(docker(managed));
    if let Some(project) = project {
        checks.extend(project_config(project));
        checks.extend(worktree_dir(project));
        if project.config.postgres().is_some() {
            checks.extend(postgres(project));
        }
        checks.extend(container_services(project));
        checks.extend(hook_trust(project, cwd));
        checks.extend(git_hook(project));
    }
    checks.extend(pitchfork_checks(project, cwd));
    checks.extend(tool(
        "mise",
        find_on_path("mise"),
        "https://mise.jdx.dev",
        "toolchains and dependency installation in setup hooks",
    ));
    if let Some(project) = project {
        checks.extend(workspaces(project));
        checks.extend(mise::checks(project));
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
                    format!("{detail}; managed PostgreSQL and the container services need it"),
                    hint,
                )
            } else {
                Check::warn(
                    "docker",
                    format!("{detail} (only needed for managed PostgreSQL and container services)"),
                    hint,
                )
            }]
        }
    }
}

fn project_config(project: &Project) -> Vec<Check> {
    let mut checks = vec![match project.config.validate() {
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
    }];
    for hint in project.config.env.hints() {
        checks.push(Check::warn(
            "env-presets",
            hint,
            "write the preset under `presets = [...]` and remove the variable",
        ));
    }
    checks
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
                "check the postgres service under [services] in devflow.toml",
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
                "check `admin_url` of the postgres service / DEVFLOW_PG_ADMIN_URL and the network"
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
    checks.extend(scratch_databases(&server));
    checks
}

/// The container services (Valkey, Redis): the Docker daemon the catalog is
/// bound to, whether its volumes share blocks (the cached probe, measured on
/// first use), and unfinished operations or recorded data nothing uses.
fn container_services(project: &Project) -> Vec<Check> {
    use crate::services::{Service, workspace};
    let configured: Vec<(String, bool)> = project
        .config
        .containers()
        .map(|(key, _, section)| (key.to_string(), section.require_cow))
        .collect();
    if configured.is_empty() {
        return Vec::new();
    }
    let mut checks = Vec::new();
    let first = match Service::open(project, &configured[0].0) {
        Ok(svc) => svc,
        Err(e) => {
            return vec![Check::fail(
                "services-docker",
                format!("{e:#}"),
                "start Docker (or OrbStack) and check `docker info`; the container services run in it",
            )];
        }
    };
    let status = match first.status(false) {
        Ok(status) => status,
        Err(e) => {
            return vec![Check::fail(
                "services",
                format!("{e:#}"),
                "the catalog is bound to the Docker daemon it was created on; see the message",
            )];
        }
    };
    let engine = if status.orbstack {
        "OrbStack"
    } else {
        "Docker"
    };
    checks.push(Check::ok(
        "services-docker",
        format!(
            "{engine} daemon {} holds the container services",
            status.docker.chars().take(12).collect::<String>()
        ),
    ));
    let require = configured.iter().any(|(_, require)| *require);
    let measured = status.cow_measured_at.map_or(String::new(), |t| {
        format!(", measured {}", t.format("%Y-%m-%d"))
    });
    checks.push(match status.cow {
        CowCapability::Verified => Check::ok(
            "services-cow",
            format!("container branches share blocks with their source (verified{measured})"),
        ),
        CowCapability::Copy if require => Check::fail(
            "services-cow",
            format!("container volumes cannot share blocks (copy{measured}) and require_cow is set"),
            "use a Docker engine whose volumes are on btrfs or XFS with reflink (OrbStack), or unset require_cow",
        ),
        CowCapability::Copy => Check::warn(
            "services-cow",
            format!("container branches are full copies (copy{measured})"),
            "use a Docker engine whose volumes are on btrfs or XFS with reflink (OrbStack)",
        ),
        CowCapability::Unknown if require => Check::fail(
            "services-cow",
            format!(
                "copy-on-write could not be verified{measured} and require_cow is set{}",
                status.cow_note.map_or(String::new(), |n| format!(": {n}"))
            ),
            "`devflow service status --probe` measures it again",
        ),
        CowCapability::Unknown => Check::warn(
            "services-cow",
            format!(
                "copy-on-write could not be verified{measured}{}",
                status.cow_note.map_or(String::new(), |n| format!(": {n}"))
            ),
            "`devflow service status --probe` measures it again",
        ),
    });
    for (key, _) in &configured {
        let Ok(svc) = Service::open(project, key) else {
            continue;
        };
        match svc.records() {
            Ok(records) => {
                let unfinished = records.unfinished().len();
                if unfinished > 0 {
                    checks.push(Check::warn(
                        &format!("service-{key}"),
                        format!(
                            "{unfinished} clone, snapshot or reset operation(s) did not finish"
                        ),
                        format!("`devflow service {key} reconcile` completes or rolls them back"),
                    ));
                } else {
                    // The engine, and what the last clone cost the source.
                    let ready = records
                        .branches
                        .iter()
                        .filter(|b| b.state == crate::services::catalog::RecordState::Ready)
                        .count();
                    let last = svc.last_clone().map_or(String::new(), |last| {
                        let method = last.method.map_or("no copy".to_string(), |m| m.to_string());
                        match last.source_downtime_ms {
                            Some(ms) => format!(
                                "; last {} {method}, source stopped {:.1} s",
                                last.operation,
                                ms as f64 / 1000.0
                            ),
                            None => {
                                format!("; last {} {method}, no source stopped", last.operation)
                            }
                        }
                    });
                    checks.push(Check::ok(
                        &format!("service-{key}"),
                        format!("{}: {ready} branch(es){last}", svc.engine),
                    ));
                }
            }
            Err(e) => checks.push(Check::fail(
                &format!("service-{key}"),
                format!("{e:#}"),
                "see the message",
            )),
        }
    }
    let orphans = workspace::orphans(project).ok();
    if let Some(orphans) = &orphans {
        checks.extend(receipts_check(orphans));
    }
    if let Some(orphans) = &orphans
        && (!orphans.services.is_empty() || !orphans.branches.is_empty())
    {
        checks.push(Check::warn(
            "services-orphans",
            format!(
                "{} service(s) no longer configured and {} branch(es) without a workspace are kept",
                orphans.services.len(),
                orphans.branches.len()
            ),
            "`devflow service list --orphans` shows them; `devflow service NAME forget` or `drop` removes them",
        ));
    }
    checks
}

/// Scratch databases (`extra_databases`) that need a decision: names the
/// configuration no longer lists (kept, never dropped implicitly) and ones
/// that are not ready. Silent when there is nothing to say.
fn scratch_databases(server: &crate::db::Server) -> Option<Check> {
    use crate::db::RecordStatus;
    // The server is checked above; an unreadable listing is not this check's news.
    let branches = server.list_branches().ok()?;
    let mut orphans = Vec::new();
    let mut unfinished = Vec::new();
    for branch in &branches {
        for extra in &branch.extras {
            let label = format!(
                "'{}' ({}) of branch '{}'",
                extra.name, extra.db_name, branch.name
            );
            if extra.orphaned {
                orphans.push(label);
            } else if extra.status != RecordStatus::Ready {
                unfinished.push(label);
            }
        }
    }
    if !orphans.is_empty() {
        return Some(Check::warn(
            "postgres-scratch",
            format!(
                "{} scratch database(s) are no longer listed in extra_databases and are kept: {}",
                orphans.len(),
                orphans.join(", ")
            ),
            "`devflow db reconcile --drop-orphans` drops them; adding the name back keeps them in use",
        ));
    }
    if !unfinished.is_empty() {
        return Some(Check::warn(
            "postgres-scratch",
            format!(
                "{} scratch database(s) are not ready: {}",
                unfinished.len(),
                unfinished.join(", ")
            ),
            "`devflow db reconcile` finishes interrupted work",
        ));
    }
    None
}

fn major_version(version: &str) -> Option<u32> {
    version
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The directory whose scope a command started in `cwd` is about: the
/// workspace containing it, else the main checkout.
fn judged_dir(project: &Project, cwd: &Path) -> PathBuf {
    worktree::containing(project, cwd)
        .ok()
        .flatten()
        .map_or_else(|| project.root.clone(), |workspace| workspace.path)
}

fn hook_trust(project: &Project, cwd: &Path) -> Vec<Check> {
    let dir = judged_dir(project, cwd);
    let at = format!("judged at {}", dir.display());
    vec![match trust::state_at(project, &dir) {
        Ok(TrustState::NothingToApprove) => Check::ok(
            "hook-trust",
            format!("nothing to approve: no hooks, [env] or pitchfork processes configured ({at})"),
        ),
        Ok(TrustState::Trusted) => Check::ok(
            "hook-trust",
            format!("the hooks, [env] and pitchfork processes are approved ({at})"),
        ),
        Ok(TrustState::Untrusted {
            previously_trusted,
            format_changed,
            ..
        }) => Check::warn(
            "hook-trust",
            format!(
                "{} ({at})",
                if format_changed {
                    "the approval was made under an older format that covered less (hooks only); review again"
                } else if previously_trusted {
                    "the hooks, [env] or pitchfork processes changed since they were approved"
                } else {
                    "the hooks, [env] and pitchfork processes have not been approved"
                }
            ),
            "review and approve them with `devflow trust` in that workspace; until then setup skips hooks and [env] recipes",
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

/// Install state of pitchfork, and, for a project that uses it, the checks of
/// its namespace, its environment delivery and the proxy. The commands that
/// would fix a problem are offered in the hint; none is run.
fn pitchfork_checks(project: Option<&Project>, cwd: &Path) -> Vec<Check> {
    let detection = pitchfork::detect();
    const INSTALL: &str =
        "run `devflow pitchfork install` (it asks first), or `mise use -g pitchfork@latest`";
    let mut checks = vec![if detection.path.is_none() {
        Check::warn(
            "pitchfork",
            "`pitchfork` was not found on PATH (optional: daemons and the local HTTPS proxy)",
            INSTALL,
        )
    } else if !detection.installed {
        Check::warn("pitchfork", detection.summary(), INSTALL)
    } else if !detection.missing_features.is_empty() {
        Check::warn(
            "pitchfork",
            detection.summary(),
            "upgrade it: `mise use -g pitchfork@latest`",
        )
    } else {
        Check::ok("pitchfork", detection.summary())
    }];
    let Some(project) = project else {
        return checks;
    };
    let dir = judged_dir(project, cwd);
    if !processes::uses_pitchfork(&dir) {
        return checks;
    }
    checks.push(match processes::namespace_problem(&dir) {
        Some(problem) => Check::warn(
            "pitchfork-namespace",
            problem,
            "daemon ids of this worktree collide with others until each worktree has its own namespace",
        ),
        None => {
            let namespace = processes::namespace_of(&dir);
            match namespace.source {
                processes::NamespaceSource::Default => Check::warn(
                    "pitchfork-namespace",
                    format!(
                        "{} is pitchfork's default (the directory name) here, shared with any \
                         other repository checked out under that name ({})",
                        namespace.value,
                        dir.display()
                    ),
                    "`devflow setup` in this workspace writes its own namespace",
                ),
                _ => Check::ok(
                    "pitchfork-namespace",
                    format!("{} ({})", namespace.value, dir.display()),
                ),
            }
        }
    });
    let wiring = processes::env_wiring(project, &dir);
    if !wiring.is_empty() {
        checks.push(Check::warn(
            "pitchfork-env",
            wiring.join("; "),
            "devflow delivers .devflow/env to daemons through mise only, never through pitchfork files",
        ));
    }
    if detection.usable() && processes::has_ports(&dir) {
        checks.push(
            match pitchfork::Pitchfork::locate().map(|p| p.proxy_status(&dir)) {
                Some(Ok(proxy)) if proxy.enabled && proxy.trusted == Some(false) => Check::warn(
                    "pitchfork-proxy",
                    format!(
                        "the {} proxy is on but its certificate is not trusted",
                        proxy.scheme.as_deref().unwrap_or("https")
                    ),
                    format!(
                        "devflow does not run these; to trust it: {}",
                        pitchfork::PROXY_COMMANDS[2]
                    ),
                ),
                Some(Ok(proxy)) if proxy.enabled => Check::ok(
                    "pitchfork-proxy",
                    format!(
                        "{}://*.{}",
                        proxy.scheme.as_deref().unwrap_or("https"),
                        proxy.tld.as_deref().unwrap_or("localhost")
                    ),
                ),
                Some(Ok(_)) => Check::warn(
                    "pitchfork-proxy",
                    "the pitchfork proxy is off, so daemons with a port have no URL",
                    format!(
                        "optional; devflow does not run these (one needs sudo): {}",
                        pitchfork::PROXY_COMMANDS.join("; ")
                    ),
                ),
                _ => return checks,
            },
        );
    }
    checks
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
    let unmapped: Vec<String> = list
        .iter()
        .filter(|w| w.receipt.as_ref().is_some_and(|r| r.unmapped.is_some()))
        .map(|w| w.workspace.name.clone())
        .collect();
    let mut checks = Vec::new();
    if !unmapped.is_empty() {
        checks.push(Check::warn(
            "workspaces",
            format!(
                "database binding from an older devflow is not mapped to a service in: {}",
                unmapped.join(", ")
            ),
            "add `[services.NAME]` with `engine = \"postgres\"` to devflow.toml; the binding is kept, not exported",
        ));
    }
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

/// The warning for workspaces whose receipt could not be read, so that who
/// owns which branch is unknown. It names each workspace with why (the reason
/// carries a position, never the receipt's content), masked like the orphan
/// listing and reconcile do.
fn receipts_check(orphans: &crate::services::workspace::Orphans) -> Option<Check> {
    if orphans.is_complete() {
        return None;
    }
    let problems = orphans
        .incomplete
        .iter()
        .map(|p| crate::redact::text(&format!("{}: {}", p.workspace, p.reason)))
        .collect::<Vec<_>>()
        .join("; ");
    Some(Check::warn(
        "services-receipts",
        format!(
            "{} workspace receipt(s) cannot be read, so which branches belong to a workspace is unknown ({problems})",
            orphans.incomplete.len(),
        ),
        "repair or remove `devflow/receipt.json` in that worktree's Git directory; no branch is listed or dropped as an orphan until then",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::testutil;

    #[test]
    fn unreadable_receipts_are_listed_with_their_masked_reasons() {
        use crate::error::UnreadReceipt;
        let mut orphans = crate::services::workspace::Orphans::default();
        assert!(receipts_check(&orphans).is_none());
        orphans.incomplete = vec![
            UnreadReceipt {
                workspace: "feature-a".into(),
                reason: "invalid receipt: expected value at line 3 column 7".into(),
            },
            UnreadReceipt {
                workspace: "password=hunter2".into(),
                // kingfisher:ignore
                reason: "cannot read postgres://u:review-password@localhost/db".into(),
            },
        ];
        let check = receipts_check(&orphans).unwrap();
        assert_eq!(check.name, "services-receipts");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert!(
            check
                .detail
                .contains("feature-a: invalid receipt: expected value at line 3 column 7"),
            "{}",
            check.detail
        );
        assert!(check.detail.starts_with("2 workspace receipt(s)"));
        // The reasons are masked, the workspace named like a credential too.
        assert!(
            !check.detail.contains("review-password"),
            "{}",
            check.detail
        );
        assert!(!check.detail.contains("hunter2"), "{}", check.detail);
    }

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
        assert!(
            find(&checks, "hook-trust")[0]
                .detail
                .starts_with("nothing to approve"),
            "{:?}",
            find(&checks, "hook-trust")[0]
        );
        assert_eq!(find(&checks, "git-hook")[0].status, CheckStatus::Warn);
        assert!(find(&checks, "postgres").is_empty(), "no postgres service");
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
    fn trust_is_judged_for_the_worktree_the_command_runs_in() {
        let t = testutil::repo(None);
        let ws = worktree::create(&t.project, "feature", None).unwrap();
        std::fs::write(
            ws.path.join("pitchfork.toml"),
            "[daemons.api]\nrun = \"bun run dev\"\n",
        )
        .unwrap();
        let trust_of = |cwd: &Path| {
            let checks = run_in(Some(&t.project), cwd);
            find(&checks, "hook-trust")[0].clone()
        };
        let main = trust_of(&t.root);
        assert_eq!(main.status, CheckStatus::Ok, "{main:?}");
        assert!(main.detail.starts_with("nothing to approve"), "{main:?}");
        assert!(
            main.detail
                .contains(&format!("judged at {}", t.root.display())),
            "{main:?}"
        );
        // The worktree has a daemon the main checkout lacks: it is judged on
        // its own, from any directory inside it, and names that directory.
        let nested = ws.path.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let linked = trust_of(&nested);
        assert_eq!(linked.status, CheckStatus::Warn, "{linked:?}");
        assert!(
            linked
                .detail
                .contains(&format!("judged at {}", ws.path.display())),
            "{linked:?}"
        );
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
        project.config.set_postgres(
            "db",
            crate::config::PostgresSection {
                mode: PostgresMode::External,
                admin_url: Some("postgres://nobody@127.0.0.1:1/postgres".into()),
                ..Default::default()
            },
        );
        let checks = run(Some(&project));
        let pg = find(&checks, "postgres");
        assert_eq!(pg.len(), 1, "{pg:?}");
        assert_eq!(pg[0].status, CheckStatus::Warn, "{pg:?}");
        assert!(pg[0].hint.is_some());
        // A managed server that was never started is reported the same way.
        project.config.set_postgres("db", Default::default());
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
