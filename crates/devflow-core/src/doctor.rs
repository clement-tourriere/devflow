//! Environment and project checks. Every check is quick and reports a problem
//! as a result instead of returning an error.
use crate::config::{CowCapability, PostgresMode};
use crate::db::CowSource;
use crate::githook::{self, InstallState};
use crate::project::Project;
use crate::receipt::Status;
use crate::remedy::{self, Remedy};
use crate::tools::{self, Probe, Tool};
use crate::trust::{self, TrustState};
use crate::{cow, pitchfork, processes, setup, worktree};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod isolation;
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
    /// What to do about it, in words that are right in any frontend: never a
    /// command line (that is what [`Check::remedies`] carry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The actions that fix it, most useful first. A frontend draws a button
    /// per remedy; the CLI prints [`Remedy::hint`] lines and the JSON document
    /// carries each with its CLI `command`.
    #[serde(
        serialize_with = "remedy::serialize_remedies",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub remedies: Vec<Remedy>,
}

impl Check {
    fn new(name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            status,
            detail: detail.into(),
            hint: None,
            remedies: Vec::new(),
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

    /// Add a remedy (the most useful first).
    fn fix(mut self, remedy: Remedy) -> Self {
        if !self.remedies.contains(&remedy) {
            self.remedies.push(remedy);
        }
        self
    }

    fn fixes(self, remedies: impl IntoIterator<Item = Remedy>) -> Self {
        remedies.into_iter().fold(self, Check::fix)
    }
}

/// git, docker/compose (managed mode), server reachability and version,
/// file_copy_method, CoW capability, worktree-directory CoW, hook trust,
/// Git hook state, pitchfork/mise presence, workspaces needing attention, and
/// whether mise hands the current workspace its own `.devflow/env` variables
/// and whether the process environment carries another workspace's
/// (`isolation`, `dotenv`). Without a project only the environment checks run.
/// Trust is judged for the main checkout; see [`run_in`] for the workspace a
/// command was started in.
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
        checks.extend(checkout(project));
        checks.extend(worktree_dir(project));
        if project.config.postgres().is_some() {
            checks.extend(postgres(project));
        }
        checks.extend(container_services(project));
        checks.extend(sqlite_services(project));
        checks.extend(recipes(project, cwd));
        checks.extend(hook_trust(project, cwd));
        checks.extend(git_hook(project));
    }
    checks.extend(pitchfork_checks(project, cwd));
    checks.extend(mise_tool(find_on_path("mise")));
    if let Some(project) = project {
        checks.extend(workspaces(project));
        checks.extend(mise::checks(project));
        checks.extend(isolation::checks(project, cwd));
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

/// The `sqlite` services: where each database is in the main checkout, the
/// library that copies it and the state of its snapshots. Reads only; no
/// database is opened.
fn sqlite_services(project: &Project) -> Vec<Check> {
    project
        .config
        .sqlite_services()
        .map(|(key, section)| {
            let name = format!("sqlite-{key}");
            let file = crate::sqlite::file_in(&project.root, section);
            let status = crate::sqlite::status(&file);
            let present = if status.exists {
                format!("{} in the main checkout", section.path)
            } else {
                format!("{} does not exist yet in the main checkout", section.path)
            };
            let library = format!("SQLite {} (bundled)", crate::sqlite::library_version());
            let store = crate::sqlite::Store::open(project.id(), key)
                .and_then(|store| Ok((store.list()?, store.unfinished()?, store.damaged()?)));
            let (snapshots, unfinished, damaged) = match store {
                Ok(found) => found,
                Err(error) => {
                    return Check::fail(
                        &name,
                        format!("{key}: the snapshots cannot be read: {error:#}"),
                        "the snapshot catalog is in the data directory; restore or remove it",
                    );
                }
            };
            let detail = format!(
                "{key}: {present}; {library}; {} snapshot{}",
                snapshots.len(),
                if snapshots.len() == 1 { "" } else { "s" }
            );
            if !damaged.is_empty() {
                Check::warn(
                    &name,
                    format!(
                        "{detail}; the files of {} are gone or changed, so their data is lost",
                        damaged.join(", ")
                    ),
                    "delete those snapshots and take them again",
                )
            } else if !unfinished.is_empty() {
                Check::warn(
                    &name,
                    format!(
                        "{detail}; {} snapshot operation{} did not finish",
                        unfinished.len(),
                        if unfinished.len() == 1 { "" } else { "s" }
                    ),
                    "the next snapshot operation of this service settles them",
                )
            } else {
                Check::ok(&name, detail)
            }
        })
        .collect()
}

/// The native checkout, when `devflow.local.toml` chose it: the `git` that
/// will run, and what choosing it authorizes. Nothing for the default.
fn checkout(project: &Project) -> Vec<Check> {
    if project.config.worktree.checkout != crate::config::CheckoutBackend::Git {
        return Vec::new();
    }
    let local = project.root.join(crate::config::LOCAL_CONFIG_FILE);
    match worktree::native::Git::locate(project) {
        Ok(git) => vec![Check::ok(
            "checkout",
            format!(
                "new worktrees are checked out by Git ({}) because {} asks for it; Git runs the \
                 repository's and your own filters, hooks and helpers, which the approval of \
                 devflow's hooks does not cover",
                git.version,
                crate::config::LOCAL_CONFIG_FILE
            ),
        )],
        Err(error) => {
            let remedies = remedy::remedies_of(&error);
            // The environment of another workspace is not a Git problem.
            let fix = if remedies.contains(&Remedy::ClearInherited) {
                "start devflow from a shell that is not inside another workspace, or let it \
                 clear the inherited variables"
            } else {
                "install Git, or remove `worktree.checkout` from devflow.local.toml"
            };
            vec![
                Check::fail("checkout", format!("{error:#}"), fix)
                    .fixes(remedies)
                    .fix(Remedy::OpenFile { path: local }),
            ]
        }
    }
}

/// What the `docker` client answered, before the daemon is asked.
enum DockerClient {
    /// No `docker` on `PATH`.
    Missing,
    /// `docker` runs but has no compose plugin.
    NoCompose,
    /// The compose version.
    Compose(String),
}

/// How long the client and the daemon get to answer; a hung daemon is a
/// failed check, not a stuck `doctor`.
const DOCKER_PROBE: std::time::Duration = std::time::Duration::from_secs(10);

fn docker(needed: bool) -> Vec<Check> {
    let mut compose = Command::new("docker");
    compose
        .args(["compose", "version", "--short"])
        .stdin(Stdio::null());
    let client = match crate::subprocess::run(compose, DOCKER_PROBE) {
        Ok(out) if out.ok => DockerClient::Compose(out.stdout.trim().to_string()),
        Ok(_) => DockerClient::NoCompose,
        Err(_) => DockerClient::Missing,
    };
    let daemon = matches!(client, DockerClient::Compose(_)).then(|| {
        let mut info = Command::new("docker");
        info.args(["info", "--format", "{{.ID}}"])
            .stdin(Stdio::null());
        match crate::subprocess::run(info, DOCKER_PROBE) {
            Ok(out) if out.ok => Ok(()),
            Ok(out) if out.timed_out => Err(format!(
                "no answer within {} seconds",
                DOCKER_PROBE.as_secs()
            )),
            Ok(out) => Err(crate::redact::text(
                out.message.as_deref().unwrap_or("`docker info` failed"),
            )),
            Err(e) => Err(crate::redact::text(&format!("{e:#}"))),
        }
    });
    docker_checks(needed, client, daemon)
}

/// The docker check: the client and compose plugin found, and the daemon
/// answering. A missing piece fails when the project needs Docker and warns
/// otherwise.
fn docker_checks(
    needed: bool,
    client: DockerClient,
    daemon: Option<Result<(), String>>,
) -> Vec<Check> {
    let (detail, hint) = match (client, daemon) {
        (DockerClient::Compose(version), Some(Ok(()))) => {
            return vec![Check::ok(
                "docker",
                format!("docker compose {version}; the daemon answers"),
            )];
        }
        (DockerClient::Compose(version), Some(Err(why))) => (
            format!("docker client found (compose {version}), daemon not reachable: {why}"),
            "start Docker (or OrbStack), or select the Docker context that is running",
        ),
        (DockerClient::Compose(version), None) => (
            format!("docker compose {version}; the daemon was not asked"),
            "start Docker (or OrbStack)",
        ),
        (DockerClient::NoCompose, _) => (
            "`docker compose` is not available".to_string(),
            "install Docker (or OrbStack) with the compose plugin",
        ),
        (DockerClient::Missing, _) => (
            "`docker` was not found on PATH".to_string(),
            "install Docker (or OrbStack) with the compose plugin",
        ),
    };
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
            "fix the file, or set the project up again",
        )
        .fixes([
            Remedy::OpenFile {
                path: project.config_path.clone(),
            },
            Remedy::Init {
                root: project.root.clone(),
            },
        ]),
    }];
    for hint in project.config.env.hints() {
        checks.push(
            Check::warn(
                "env-presets",
                hint.message,
                "write the preset under `presets = [...]` and remove the variable",
            )
            .fix(Remedy::AddEnvPreset {
                root: project.root.clone(),
                preset: hint.preset.to_string(),
            }),
        );
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
        let (hint, remedy) = match server.mode() {
            PostgresMode::Managed => (
                "the managed server is not running",
                Some(Remedy::StartDatabase {
                    root: project.root.clone(),
                }),
            ),
            PostgresMode::External => (
                "check `admin_url` of the postgres service / DEVFLOW_PG_ADMIN_URL and the network",
                None,
            ),
        };
        let detail = status
            .problem
            .clone()
            .unwrap_or_else(|| format!("{endpoint} is not reachable"));
        checks.push(Check::warn("postgres", detail, hint).fixes(remedy));
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
    checks.extend(scratch_databases(project, &server));
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
            "measure it again",
        )
        .fix(measure(project, &configured[0].0)),
        CowCapability::Unknown => Check::warn(
            "services-cow",
            format!(
                "copy-on-write could not be verified{measured}{}",
                status.cow_note.map_or(String::new(), |n| format!(": {n}"))
            ),
            "measure it again",
        )
        .fix(measure(project, &configured[0].0)),
    });
    for (key, _) in &configured {
        let Ok(svc) = Service::open(project, key) else {
            continue;
        };
        match svc.records() {
            Ok(records) => {
                let unfinished = records.unfinished().len();
                if unfinished > 0 {
                    checks.push(
                        Check::warn(
                            &format!("service-{key}"),
                            format!(
                                "{unfinished} clone, snapshot or reset operation(s) did not finish"
                            ),
                            "finishing or rolling them back settles them",
                        )
                        .fix(Remedy::ReconcileService {
                            root: project.root.clone(),
                            key: key.clone(),
                        }),
                    );
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
        checks.push(
            Check::warn(
                "services-orphans",
                format!(
                    "{} service(s) no longer configured and {} branch(es) without a workspace are kept",
                    orphans.services.len(),
                    orphans.branches.len()
                ),
                "they stay until you forget the service or drop the branch",
            )
            .fixes(orphans.services.iter().map(|key| Remedy::ForgetService {
                root: project.root.clone(),
                key: key.clone(),
            }))
            .fixes(orphans.branches.iter().map(|branch| Remedy::DropServiceBranch {
                root: project.root.clone(),
                key: branch.service.clone(),
                branch: branch.branch,
            })),
        );
    }
    checks
}

/// Scratch databases (`extra_databases`) that need a decision: names the
/// configuration no longer lists (kept, never dropped implicitly) and ones
/// that are not ready. Silent when there is nothing to say.
fn scratch_databases(project: &Project, server: &crate::db::Server) -> Option<Check> {
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
        return Some(
            Check::warn(
                "postgres-scratch",
                format!(
                    "{} scratch database(s) are no longer listed in extra_databases and are kept: {}",
                    orphans.len(),
                    orphans.join(", ")
                ),
                "dropping them frees the space; adding the name back to extra_databases keeps them in use",
            )
            .fix(Remedy::DropOrphanDatabases {
                root: project.root.clone(),
            }),
        );
    }
    if !unfinished.is_empty() {
        return Some(
            Check::warn(
                "postgres-scratch",
                format!(
                    "{} scratch database(s) are not ready: {}",
                    unfinished.len(),
                    unfinished.join(", ")
                ),
                "interrupted work has to be finished",
            )
            .fix(Remedy::ReconcileDatabases {
                root: project.root.clone(),
            }),
        );
    }
    None
}

fn measure(project: &Project, key: &str) -> Remedy {
    Remedy::MeasureServices {
        root: project.root.clone(),
        key: key.to_string(),
    }
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
            "review and approve them in that workspace; until then setup skips hooks and [env] recipes",
        )
        .fix(Remedy::Trust {
            workspace: dir.clone(),
        }),
        Err(e) => Check::fail(
            "hook-trust",
            format!("{e:#}"),
            "check trust.toml in the devflow config directory",
        ),
    }]
}

/// Hosts the repository is used with whose worktrees devflow does not set up
/// (the host's own script is the dependable way: it runs in the new worktree
/// whatever Git hooks that checkout has), and a relative `core.hooksPath` that
/// keeps the Git hook from firing in a new worktree.
fn host_checks(project: &Project) -> Vec<Check> {
    use crate::integrate::{self, State};
    let mut checks = Vec::new();
    let hosts: Vec<_> = integrate::suggestions(project);
    let integrate_fixes = |hosts: &[integrate::Detected]| -> Vec<Remedy> {
        hosts
            .iter()
            .map(|detected| Remedy::IntegrateHost {
                root: project.root.clone(),
                host: detected.host.name().to_string(),
            })
            .collect()
    };
    if let Ok(Some(relative)) = githook::relative_hooks_path(project)
        && !relative.committed
    {
        let shown = relative.configured.display();
        let message = if relative.husky {
            format!(
                "core.hooksPath is the relative path {shown}, which husky generates and Git ignores: a worktree made by git worktree add has no hook there, so the post-checkout hook never fires"
            )
        } else {
            format!(
                "core.hooksPath is the relative path {shown}; Git looks for it inside each worktree, and a post-checkout hook there is not part of a new checkout, so it never fires"
            )
        };
        let mut check = Check::warn(
            "git-hook-path",
            message,
            "set worktrees up from the tool that creates them (its devflow integration), commit the hook, or point core.hooksPath at an absolute directory",
        );
        for fix in integrate_fixes(&hosts) {
            check = check.fix(fix);
        }
        checks.push(check);
    }
    for detected in hosts {
        let host = detected.host;
        let message = format!(
            "{} is used here ({}) but its worktrees are not set up by devflow",
            host.label(),
            detected.found.join(", ")
        );
        let hint = match detected.state {
            State::Conflict => {
                "an entry of someone else's stands in the way of devflow's; the integration reports it and leaves it alone"
            }
            _ => {
                "writing devflow's setup into the host's project file makes it set new worktrees up"
            }
        };
        checks.push(
            Check::warn(&format!("integration-{}", host.name()), message, hint).fix(
                Remedy::IntegrateHost {
                    root: project.root.clone(),
                    host: host.name().to_string(),
                },
            ),
        );
    }
    checks
}

fn git_hook(project: &Project) -> Vec<Check> {
    let mut checks = git_hook_state(project);
    checks.extend(host_checks(project));
    checks
}

fn git_hook_state(project: &Project) -> Vec<Check> {
    vec![match githook::state(project) {
        // An unreleased build moved somebody's hook aside and wrapped it.
        Ok(InstallState::Installed { path }) if githook::legacy_original(&path).is_some() => {
            let original = githook::legacy_original(&path).unwrap_or_default();
            Check::warn(
                "git-hook",
                format!(
                    "post-checkout at {} is a wrapper from an older devflow that runs the hook it replaced ({}) first",
                    path.display(),
                    original.display()
                ),
                "it still works; removing devflow's Git hook (devflow hook uninstall) puts your original hook back",
            )
        }
        Ok(InstallState::Installed { path }) => Check::ok(
            "git-hook",
            format!("post-checkout installed at {}", path.display()),
        ),
        Ok(InstallState::NotInstalled) => Check::warn(
            "git-hook",
            "worktrees created by other tools (git worktree add, IDEs, agents) are not set up automatically",
            "installing the Git hook sets them up",
        )
        .fix(Remedy::InstallGitHook {
            root: project.root.clone(),
        }),
        Ok(InstallState::Foreign { path }) => {
            // Somebody put devflow's call into their own hook by hand.
            let added =
                std::fs::read_to_string(&path).is_ok_and(|t| t.contains("--from-git-hook"));
            if added {
                Check::ok(
                    "git-hook",
                    format!("{} sets up new worktrees through devflow", path.display()),
                )
            } else {
                Check::warn(
                    "git-hook",
                    format!(
                        "{} is a post-checkout hook from another tool; devflow does not edit it",
                        path.display()
                    ),
                    "add devflow's line to it so new worktrees are set up",
                )
                .fixes([
                    Remedy::CopyHookLine { path: path.clone() },
                    Remedy::OpenFile { path },
                ])
            }
        }
        Err(e) => Check::fail("git-hook", format!("{e:#}"), "check core.hooksPath"),
    }]
}

/// Is there a pitchfork, does it run, and is it a release devflow supports
/// ([`crate::tools::MIN_PITCHFORK`])? A release that is older, or whose version
/// cannot be read, is the typed problem the gates refuse with, and carries the
/// way to fix it.
fn pitchfork_install_check(detection: &pitchfork::Detection) -> Check {
    const INSTALL: &str = "install it (devflow asks first, and does it through mise)";
    if detection.path.is_none() {
        Check::warn(
            "pitchfork",
            "`pitchfork` was not found on PATH (optional: daemons and the local HTTPS proxy)",
            INSTALL,
        )
        .fix(Remedy::InstallPitchfork)
    } else if !detection.installed {
        Check::warn("pitchfork", detection.summary(), INSTALL).fix(Remedy::InstallPitchfork)
    } else if let Some(problem) = &detection.unsupported {
        Check::warn(
            "pitchfork",
            detection.summary(),
            "upgrade it (devflow asks first, and does it through mise)",
        )
        .fixes(problem.remedies())
    } else {
        Check::ok("pitchfork", detection.summary())
    }
}

/// Install state of pitchfork, and, for a project that uses it, the checks of
/// its namespace, its environment delivery and the proxy. The commands that
/// would fix a problem are offered in the hint; none is run.
fn pitchfork_checks(project: Option<&Project>, cwd: &Path) -> Vec<Check> {
    let detection = pitchfork::detect();
    let mut checks = vec![pitchfork_install_check(&detection)];
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
        )
        .fix(Remedy::Setup {
            workspace: dir.clone(),
        }),
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
                    "setting this workspace up again writes its own namespace",
                )
                .fix(Remedy::Setup {
                    workspace: dir.clone(),
                }),
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

/// mise on `PATH`: found, runs, and a release devflow supports
/// ([`tools::MIN_MISE`]). A release that is older, or that names none, is the
/// typed problem the hook gate refuses with, and carries the page that says how
/// to upgrade it.
fn mise_tool(found: Option<PathBuf>) -> Vec<Check> {
    let Some(path) = found else {
        return vec![Check::warn(
            "mise",
            "`mise` was not found on PATH (optional: toolchains and dependency installation in \
             setup hooks)",
            "install it from https://mise.jdx.dev",
        )];
    };
    vec![match tools::probe(Tool::Mise, &path) {
        Probe::Supported(version) => {
            Check::ok("mise", format!("mise {version} at {}", path.display()))
        }
        Probe::Unsupported(problem) => Check::warn(
            "mise",
            format!("{} (at {})", problem.message(), path.display()),
            "upgrade mise (setup hooks that run it are not run until it is a supported release)",
        )
        .fixes(problem.remedies()),
        Probe::DoesNotRun(why) => Check::warn(
            "mise",
            format!("{} is on PATH but does not run: {why}", path.display()),
            "fix the installation, or install mise again from https://mise.jdx.dev",
        )
        .fix(Remedy::OpenGuide {
            url: Tool::Mise.upgrade_page().to_string(),
        }),
    }]
}

fn workspaces(project: &Project) -> Vec<Check> {
    let list = match setup::list(project) {
        Ok(list) => list,
        Err(e) => {
            return vec![Check::fail(
                "workspaces",
                format!("{e:#}"),
                "check that the repository can be read",
            )];
        }
    };
    let attention: Vec<&setup::WorkspaceStatus> = list
        .iter()
        .filter(|w| matches!(w.status, Some(Status::NeedsAttention)))
        .collect();
    let interrupted: Vec<&setup::WorkspaceStatus> = list
        .iter()
        .filter(|w| matches!(w.status, Some(Status::InProgress)) && !w.busy)
        .collect();
    let names = |some: &[&setup::WorkspaceStatus]| {
        some.iter()
            .map(|w| w.workspace.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing: Vec<&setup::WorkspaceStatus> =
        list.iter().filter(|w| w.workspace.missing).collect();
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
        checks.push(
            Check::warn(
                "workspaces",
                format!("needs attention: {}", names(&attention)),
                "setting each up again shows what failed and resumes it",
            )
            .fixes(attention.iter().flat_map(|w| w.remedies(project))),
        );
    }
    if !interrupted.is_empty() {
        checks.push(
            Check::warn(
                "workspaces",
                format!("setup was interrupted in: {}", names(&interrupted)),
                "setting each up again resumes it",
            )
            .fixes(interrupted.iter().flat_map(|w| w.remedies(project))),
        );
    }
    if !missing.is_empty() {
        checks.push(
            Check::warn(
                "workspaces",
                format!("directory missing for: {}", names(&missing)),
                "a worktree on a drive that is not connected looks the same, so connect it first; otherwise review the registrations to forget (those that own data are kept), or remove the workspaces",
            )
            .fix(Remedy::PruneWorktrees {
                root: project.root.clone(),
            })
            .fixes(missing.iter().map(|w| Remedy::RemoveWorkspace {
                root: project.root.clone(),
                name: w.workspace.name.clone(),
            })),
        );
    }
    // Workspaces Git forgot (or whose directory is gone) that still own
    // resources, from the ledger's records alone.
    if let Ok(gone) = crate::gc::summary(project) {
        if !gone.removed_elsewhere.is_empty() {
            checks.push(
                Check::warn(
                    "workspaces-gone",
                    format!(
                        "removed outside devflow, still owning data or processes: {}",
                        gone.removed_elsewhere.join(", ")
                    ),
                    "git worktree remove, and the agent hosts that delete worktrees themselves, leave the data branches, containers and daemons behind; review them and remove the ones you do not need",
                )
                .fix(Remedy::CleanUp {
                    root: project.root.clone(),
                }),
            );
        }
        if !gone.missing.is_empty() {
            checks.push(
                Check::warn(
                    "workspaces-gone",
                    format!(
                        "directory missing, still owning data or processes: {}",
                        gone.missing.join(", ")
                    ),
                    "a worktree on a drive that is not connected looks the same, so connect it first; otherwise review them and remove the ones you do not need",
                )
                .fix(Remedy::CleanUp {
                    root: project.root.clone(),
                }),
            );
        }
        if gone.unreadable > 0 {
            checks.push(Check::warn(
                "workspaces-gone",
                format!(
                    "{} workspace record(s) cannot be read, so what those workspaces own is unknown",
                    gone.unreadable
                ),
                "repair or remove the unreadable files in `devflow/workspaces` of the repository's Git directory; nothing they might name is dropped meanwhile",
            ));
        }
    }
    if checks.is_empty() {
        checks.push(Check::ok(
            "workspaces",
            format!("{} workspace(s), none need attention", list.len()),
        ));
    }
    // A branch's own devflow.toml never reconfigures the project: the shared
    // sections it changes are reported, not applied.
    for status in &list {
        let Some(report) = &status.recipes else {
            continue;
        };
        for differing in &report.differing {
            checks.push(
                Check::warn(
                    "shared-sections",
                    format!(
                        "workspace '{}': {}",
                        status.workspace.name,
                        differing.warning()
                    ),
                    "the project's services, data and integrations are read from the main \
                     checkout; change them there",
                )
                .fix(Remedy::OpenFile {
                    path: report.shared.clone(),
                }),
            );
        }
        if let Some(local) = &report.ignored_local {
            checks.push(Check::warn(
                "local-config",
                format!(
                    "workspace '{}': {} is ignored; devflow.local.toml is read from the main \
                     checkout and applies to every workspace",
                    status.workspace.name,
                    local.display()
                ),
                "move what it holds to the main checkout's devflow.local.toml",
            ));
        }
    }
    checks
}

/// Where the recipes of the workspace a command started in `cwd` come from:
/// its own `devflow.toml`, the main checkout's, or none (a missing or
/// unusable file, which needs a choice).
fn recipes(project: &Project, cwd: &Path) -> Vec<Check> {
    use crate::effective::RecipesSource;
    let dir = judged_dir(project, cwd);
    let at = project.at(&dir);
    let effective = at.effective();
    let source = effective.recipes.describe();
    let shared = format!(
        "the project's services, data and integrations come from {}{}",
        effective.shared.display(),
        effective
            .local
            .as_ref()
            .map(|local| format!(" with {}", local.display()))
            .unwrap_or_default()
    );
    let judged = format!("judged at {}", dir.display());
    vec![match &effective.recipes {
        RecipesSource::Main { .. } | RecipesSource::Workspace { .. } => Check::ok(
            "recipes",
            format!("hooks, [env] and the copy list come from {source}; {shared} ({judged})"),
        ),
        RecipesSource::DefaultChosen { because, .. } => Check::warn(
            "recipes",
            format!(
                "this workspace uses {source}: {}; {shared} ({judged})",
                because.message
            ),
            "commit a valid devflow.toml in the workspace to use its own recipes; the choice is \
             cleared then",
        )
        .fix(Remedy::OpenFile {
            path: because.file.clone(),
        }),
        RecipesSource::Unavailable { because } => Check::fail(
            "recipes",
            format!(
                "this workspace has no usable recipes: {}; no hooks, [env] or copy list apply \
                 ({judged})",
                because.message
            ),
            "commit a valid devflow.toml in the workspace, or use the default workspace's recipes",
        )
        .fix(Remedy::UseDefaultRecipes {
            workspace: dir.clone(),
        })
        .fixes(
            (because.kind != crate::effective::ProblemKind::Missing).then(|| Remedy::OpenFile {
                path: because.file.clone(),
            }),
        ),
    }]
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

    #[test]
    fn a_docker_client_without_a_daemon_fails_when_docker_is_needed_and_warns_otherwise() {
        let down = || Some(Err("Cannot connect to the Docker daemon".to_string()));
        let compose = || DockerClient::Compose("2.40.0".into());
        let needed = docker_checks(true, compose(), down());
        assert_eq!(needed[0].status, CheckStatus::Fail);
        assert!(
            needed[0].detail.contains("daemon not reachable")
                && needed[0].detail.contains("Cannot connect"),
            "{:?}",
            needed[0]
        );
        let optional = docker_checks(false, compose(), down());
        assert_eq!(optional[0].status, CheckStatus::Warn);
        assert!(optional[0].detail.contains("daemon not reachable"));
        // Reachable: fine whatever the project needs; no client: same rule.
        assert_eq!(
            docker_checks(true, compose(), Some(Ok(())))[0].status,
            CheckStatus::Ok
        );
        assert_eq!(
            docker_checks(true, DockerClient::Missing, None)[0].status,
            CheckStatus::Fail
        );
        assert_eq!(
            docker_checks(false, DockerClient::NoCompose, None)[0].status,
            CheckStatus::Warn
        );
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
    fn a_relative_hooks_path_that_a_new_worktree_lacks_is_explained_with_the_host_integration() {
        let t = testutil::repo(None);
        // husky: generated and ignored, so a new worktree has nothing there.
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.hooksPath", ".husky/_")
            .unwrap();
        std::fs::write(t.root.join("t3.json"), "{}").unwrap();
        let project = t.project.reload().unwrap();
        githook::install(&project).unwrap();
        let checks = run(Some(&project));
        let path = find(&checks, "git-hook-path");
        assert_eq!(path.len(), 1, "{checks:?}");
        assert_eq!(path[0].status, CheckStatus::Warn);
        assert!(path[0].detail.contains("husky"), "{}", path[0].detail);
        assert!(path[0].detail.contains(".husky/_"), "{}", path[0].detail);
        assert!(path[0].detail.contains("never fires"));
        assert_eq!(
            path[0].remedies,
            [Remedy::IntegrateHost {
                root: t.root.clone(),
                host: "t3".into()
            }]
        );
        // The host that is used here is reported in its own right.
        let host = find(&checks, "integration-t3");
        assert_eq!(host.len(), 1);
        assert!(host[0].detail.contains("T3 Code is used here (t3.json)"));
        for check in path.iter().chain(host.iter()) {
            for text in [Some(&check.detail), check.hint.as_ref()]
                .into_iter()
                .flatten()
            {
                assert!(!has_cli_text(text), "{}: {text}", check.name);
            }
        }

        // Once the host is set up, only the hook path stays.
        let change = crate::integrate::plan(&t.root, crate::integrate::Host::T3, false).unwrap();
        crate::integrate::apply(&change).unwrap();
        let checks = run(Some(&project));
        assert!(find(&checks, "integration-t3").is_empty());
        assert_eq!(find(&checks, "git-hook-path").len(), 1);
    }

    #[test]
    fn a_relative_hooks_path_with_a_committed_hook_is_fine() {
        let t = testutil::repo(None);
        std::fs::create_dir_all(t.root.join("tools/hooks")).unwrap();
        std::fs::write(t.root.join("tools/hooks/post-checkout"), "#!/bin/sh\n").unwrap();
        testutil::commit_all(&t.project.repo().unwrap(), "hooks");
        t.project
            .repo()
            .unwrap()
            .config()
            .unwrap()
            .set_str("core.hooksPath", "tools/hooks")
            .unwrap();
        let checks = run(Some(&t.project.reload().unwrap()));
        assert!(find(&checks, "git-hook-path").is_empty(), "{checks:?}");
    }

    #[test]
    fn a_hook_somebody_else_wrote_is_offered_the_line_not_replaced() {
        let t = testutil::repo(None);
        let hook = t.root.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\necho mine\n").unwrap();
        let checks = run(Some(&t.project));
        let check = find(&checks, "git-hook")[0];
        assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
        assert_eq!(
            check.remedies,
            [
                Remedy::CopyHookLine { path: hook.clone() },
                Remedy::OpenFile { path: hook.clone() }
            ]
        );
        assert!(check.detail.contains("devflow does not edit it"));
        for text in [Some(&check.detail), check.hint.as_ref()]
            .into_iter()
            .flatten()
        {
            assert!(!has_cli_text(text), "{text}");
            assert!(!text.contains("--from-git-hook"), "{text}");
        }
    }

    #[test]
    fn a_hook_with_devflows_line_added_by_hand_is_configured() {
        let t = testutil::repo(None);
        let hook = t.root.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho mine\n{}\n", githook::HOOK_LINE),
        )
        .unwrap();
        let checks = run(Some(&t.project));
        let check = find(&checks, "git-hook")[0];
        assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
        assert!(check.remedies.is_empty());
        assert!(!check.detail.contains("echo mine"), "{}", check.detail);
    }

    #[test]
    fn a_sqlite_service_is_checked_without_opening_its_database() {
        let text = format!(
            "[project]\nname = \"shop\"\nid = \"{}\"\n\n[services.app]\nengine = \"sqlite\"\n\
             path = \"db/app.sqlite3\"\n",
            uuid::Uuid::new_v4()
        );
        let t = testutil::repo(Some(&text));
        let checks = run(Some(&t.project));
        let check = find(&checks, "sqlite-app")[0];
        assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
        assert!(
            check.detail.contains("does not exist yet"),
            "{}",
            check.detail
        );
        assert!(check.detail.contains("bundled"), "{}", check.detail);
        assert!(check.detail.contains("0 snapshots"), "{}", check.detail);
        // A database in WAL mode is looked at, not opened: no file appears.
        let file = t.root.join("db/app.sqlite3");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute_batch("CREATE TABLE t (n)").unwrap();
        drop(conn);
        let listing = || {
            let mut names: Vec<_> = std::fs::read_dir(file.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let before = listing();
        let checks = run(Some(&t.project));
        assert_eq!(find(&checks, "sqlite-app")[0].status, CheckStatus::Ok);
        assert!(
            find(&checks, "sqlite-app")[0]
                .detail
                .contains("app.sqlite3 in the main checkout")
        );
        assert_eq!(listing(), before, "no -wal or -shm appeared");
    }

    #[test]
    fn the_checkout_check_appears_only_when_the_local_file_chose_git() {
        let t = testutil::repo(None);
        assert!(find(&run(Some(&t.project)), "checkout").is_empty());
        std::fs::write(
            t.root.join("devflow.local.toml"),
            "[worktree]\ncheckout = \"git\"\n",
        )
        .unwrap();
        let project = Project::open(&t.root).unwrap();
        let checks = run(Some(&project));
        let check = find(&checks, "checkout")[0];
        if find_on_path("git").is_some() {
            assert_eq!(check.status, CheckStatus::Ok, "{check:?}");
            assert!(check.detail.contains("git version"), "{}", check.detail);
            assert!(
                check.detail.contains("devflow.local.toml"),
                "{}",
                check.detail
            );
        } else {
            assert_eq!(check.status, CheckStatus::Fail, "{check:?}");
            assert!(
                check
                    .remedies
                    .iter()
                    .any(|r| matches!(r, Remedy::OpenGuide { .. }))
            );
        }
        assert!(!check.detail.contains("devflow trust"), "no command line");
    }

    #[test]
    fn a_wrapper_from_an_older_build_is_reported_in_plain_words() {
        let t = testutil::repo(None);
        let hook = t.root.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            "#!/bin/sh\n# devflow-managed-hook v2\n# devflow-wraps-original post-checkout.devflow-original\nexit 0\n",
        )
        .unwrap();
        std::fs::write(
            hook.with_file_name("post-checkout.devflow-original"),
            "#!/bin/sh\necho mine\n",
        )
        .unwrap();
        let checks = run(Some(&t.project));
        let check = find(&checks, "git-hook")[0];
        assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
        assert!(check.detail.contains("wrapper from an older devflow"));
        assert!(check.detail.contains("post-checkout.devflow-original"));
        assert!(!check.detail.contains("echo mine"), "{}", check.detail);
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
    fn workspaces_git_forgot_are_reported_with_the_clean_up_remedy_and_no_command() {
        let t = testutil::repo(None);
        let ws = worktree::create(&t.project, "agent-made", None).unwrap();
        let mut receipt = crate::receipt::Receipt::new(t.project.id(), "agent-made", None);
        receipt.processes = Some(crate::receipt::ProcessRecord {
            started_at: chrono::Utc::now(),
            ids: vec![format!("{}/web", receipt.slug())],
        });
        receipt.save(&ws.git_dir).unwrap();
        // What an agent host does when it is done: remove the worktree.
        worktree::force_remove(&t.project, &ws).unwrap();

        let checks = run(Some(&t.project));
        let found: Vec<&Check> = find(&checks, "workspaces-gone");
        assert_eq!(found.len(), 1, "{checks:?}");
        assert_eq!(found[0].status, CheckStatus::Warn);
        assert!(found[0].detail.contains("agent-made"), "{:?}", found[0]);
        assert!(
            !has_cli_text(&found[0].detail) && !has_cli_text(found[0].hint.as_deref().unwrap())
        );
        assert_eq!(
            found[0].remedies,
            [Remedy::CleanUp {
                root: t.project.root.clone()
            }]
        );
        // Nothing owned, nothing to say.
        let quiet = testutil::repo(None);
        let ws = worktree::create(&quiet.project, "tidy", None).unwrap();
        crate::receipt::Receipt::new(quiet.project.id(), "tidy", None)
            .save(&ws.git_dir)
            .unwrap();
        worktree::force_remove(&quiet.project, &ws).unwrap();
        assert!(find(&run(Some(&quiet.project)), "workspaces-gone").is_empty());
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

    /// Text a person reads in a frontend: never a command line of the CLI.
    /// (`devflow exec` inside a hook command is configuration, not a command
    /// to run, and stays in the mise checks.)
    fn has_cli_text(text: &str) -> bool {
        ["`devflow ", "run devflow", "`git worktree prune"]
            .iter()
            .any(|needle| text.contains(needle))
    }

    #[test]
    fn checks_carry_their_fixes_as_remedies_and_say_nothing_of_the_cli() {
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
        let gone = worktree::create(&t.project, "gone", None).unwrap();
        std::fs::remove_dir_all(&gone.path).unwrap();

        let checks = run(Some(&t.project));
        for check in &checks {
            for text in [Some(&check.detail), check.hint.as_ref()]
                .into_iter()
                .flatten()
            {
                assert!(!has_cli_text(text), "{}: {text}", check.name);
            }
        }
        assert_eq!(
            find(&checks, "git-hook")[0].remedies,
            [Remedy::InstallGitHook {
                root: t.root.clone()
            }]
        );
        let workspaces = find(&checks, "workspaces");
        let attention = workspaces
            .iter()
            .find(|c| c.detail.contains("needs attention"))
            .expect("a workspace needs attention");
        assert!(
            attention.remedies.contains(&Remedy::Setup {
                workspace: ws.path.clone()
            }),
            "{attention:?}"
        );
        let missing = workspaces
            .iter()
            .find(|c| c.detail.contains("directory missing"))
            .expect("a directory is missing");
        assert_eq!(
            missing.remedies,
            [
                Remedy::PruneWorktrees {
                    root: t.root.clone()
                },
                Remedy::RemoveWorkspace {
                    root: t.root.clone(),
                    name: "gone".into()
                }
            ]
        );
        // The JSON document carries each remedy with its CLI line.
        let json = serde_json::to_value(attention).unwrap();
        assert_eq!(json["remedies"][0]["kind"], "setup");
        assert_eq!(json["remedies"][0]["command"], "devflow setup");
        assert!(json["hint"].as_str().is_some_and(|h| !has_cli_text(h)));
        // A check without a fix has no `remedies` key at all.
        let ok = serde_json::to_value(find(&checks, "git")[0]).unwrap();
        assert!(ok.get("remedies").is_none());
    }

    #[test]
    fn an_unreachable_managed_server_offers_to_start_it() {
        let t = testutil::repo(None);
        let mut project = t.project.clone();
        project.config.set_postgres("db", Default::default());
        let checks = run(Some(&project));
        let pg = find(&checks, "postgres");
        if pg[0].status == CheckStatus::Warn {
            assert!(
                pg[0].remedies.contains(&Remedy::StartDatabase {
                    root: project.root.clone()
                }),
                "{pg:?}"
            );
        }
    }

    #[test]
    fn parses_server_major_versions() {
        assert_eq!(major_version("18.0"), Some(18));
        assert_eq!(major_version("PostgreSQL 17.4 (Debian)"), Some(17));
        assert_eq!(major_version("unknown"), None);
    }
    /// A stand-in `mise` that prints `stdout` for `--version` (or exits 1).
    #[cfg(unix)]
    fn stand_in_mise(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("mise");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn guide_url(check: &Check) -> Option<&str> {
        check.remedies.iter().find_map(|r| match r {
            Remedy::OpenGuide { url } => Some(url.as_str()),
            _ => None,
        })
    }

    #[cfg(unix)]
    #[test]
    fn mise_is_judged_against_the_pin_with_the_upgrade_page_as_the_way_on() {
        let dir = tempfile::tempdir().unwrap();
        // Supported: the release is shown.
        let ok = mise_tool(Some(stand_in_mise(
            dir.path(),
            "echo '2026.10.7 macos-arm64 (2026-10-09)'",
        )));
        assert_eq!(ok[0].status, CheckStatus::Ok);
        assert!(
            ok[0].detail.starts_with("mise 2026.10.7 at "),
            "{}",
            ok[0].detail
        );
        assert!(ok[0].remedies.is_empty());
        // The pin itself is supported.
        let pin = mise_tool(Some(stand_in_mise(
            dir.path(),
            "echo '2026.4.18 linux-x64 (2026-04-19)'",
        )));
        assert_eq!(pin[0].status, CheckStatus::Ok);
        // Older: a typed warning with the page, no command line in the words.
        let old = mise_tool(Some(stand_in_mise(
            dir.path(),
            "echo '2026.4.17 linux-x64 (2026-04-18)'",
        )));
        assert_eq!(old[0].status, CheckStatus::Warn);
        assert!(
            old[0]
                .detail
                .contains("mise 2026.4.17 is older than 2026.4.18"),
            "{}",
            old[0].detail
        );
        assert_eq!(
            guide_url(&old[0]),
            Some("https://mise.jdx.dev/installing-mise.html")
        );
        assert!(!old[0].hint.as_deref().unwrap_or("").contains('`'));
        // A version that cannot be read is a warning too, not "fine".
        let unreadable = mise_tool(Some(stand_in_mise(dir.path(), "echo 'dev build'")));
        assert_eq!(unreadable[0].status, CheckStatus::Warn);
        assert!(
            unreadable[0].detail.contains("cannot be read"),
            "{}",
            unreadable[0].detail
        );
        assert!(guide_url(&unreadable[0]).is_some());
        // On PATH and not running (a shim with no version configured).
        let shim = mise_tool(Some(stand_in_mise(
            dir.path(),
            "echo 'mise ERROR No version is set for shim: mise' >&2; exit 1",
        )));
        assert_eq!(shim[0].status, CheckStatus::Warn);
        assert!(
            shim[0].detail.contains("does not run"),
            "{}",
            shim[0].detail
        );
        // Not on PATH at all.
        let absent = mise_tool(None);
        assert_eq!(absent[0].status, CheckStatus::Warn);
        assert!(absent[0].detail.contains("not found on PATH"));
    }

    #[test]
    fn pitchfork_is_judged_against_the_pin_and_names_how_to_upgrade_it() {
        let detection = |version: Option<&str>| pitchfork::Detection {
            path: Some(PathBuf::from("/usr/local/bin/pitchfork")),
            installed: true,
            version: version.map(str::to_string),
            unsupported: crate::tools::judge(Tool::Pitchfork, version.unwrap_or("")).err(),
            problem: None,
        };
        let ok = pitchfork_install_check(&detection(Some("2.30.1")));
        assert_eq!(ok.status, CheckStatus::Ok);
        let old = pitchfork_install_check(&detection(Some("2.19.0")));
        assert_eq!(old.status, CheckStatus::Warn);
        assert!(
            old.detail.contains("pitchfork 2.19.0 is older than 2.20.0"),
            "{}",
            old.detail
        );
        assert_eq!(old.remedies[0], Remedy::InstallPitchfork);
        assert_eq!(
            guide_url(&old),
            Some("https://github.com/jdx/pitchfork/releases")
        );
        assert!(!old.hint.as_deref().unwrap_or("").contains('`'));
        let unreadable = pitchfork_install_check(&detection(None));
        assert_eq!(unreadable.status, CheckStatus::Warn);
        assert!(
            unreadable.detail.contains("cannot be read"),
            "{}",
            unreadable.detail
        );
        // Missing, or on PATH and not running: the install offer, no command in the words.
        let missing = pitchfork_install_check(&pitchfork::Detection {
            path: None,
            installed: false,
            version: None,
            unsupported: None,
            problem: None,
        });
        assert_eq!(missing.remedies, [Remedy::InstallPitchfork]);
        assert!(!missing.hint.as_deref().unwrap_or("").contains('`'));
    }
}
