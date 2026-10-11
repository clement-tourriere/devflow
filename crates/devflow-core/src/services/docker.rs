//! The `docker` command line, as the container services use it: daemon
//! identity, image pinning, containers, volumes and the short-lived helper
//! containers that clone files inside the Docker VM.
//!
//! Everything goes through the `docker` client, so `DOCKER_HOST`,
//! `DOCKER_CONTEXT` and remote engines work as they do for the user. Nothing
//! here decides who owns a resource: callers pass identities from the catalog
//! and use [`ContainerInfo::labels`] only to corroborate them.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

/// Label every resource devflow creates carries, with the project id.
pub const LABEL_PROJECT: &str = "dev.devflow.project";
pub const LABEL_SERVICE: &str = "dev.devflow.service";
pub const LABEL_BRANCH: &str = "dev.devflow.branch";
pub const LABEL_SNAPSHOT: &str = "dev.devflow.snapshot";
pub const LABEL_ENGINE: &str = "dev.devflow.engine";
pub const LABEL_VERSION: &str = "dev.devflow.version";
pub const LABEL_HELPER: &str = "dev.devflow.helper";
/// On a tombstone: a container that is never started and only holds the name
/// of a helper attempt whose outcome is unknown, with the operation's id (see
/// `helpers.rs`). It deliberately has no [`LABEL_HELPER`]: it is not a helper.
pub const LABEL_TOMBSTONE: &str = "dev.devflow.tombstone";
/// Set from `DEVFLOW_TEST_LABEL` on everything the Docker tests create, so
/// a test run can find and remove its leftovers.
pub const LABEL_TEST: &str = "dev.devflow.test";
/// The label OrbStack turns into a DNS name for the container.
pub const LABEL_ORBSTACK_DOMAINS: &str = "dev.orbstack.domains";

/// Image of the helper containers that copy files inside the Docker VM: it
/// has GNU `cp`, which supports `--reflink=always` (BusyBox's behavior varies).
pub const HELPER_IMAGE: &str = "debian:stable-slim";

/// What the catalog says a Docker resource is: the labels it must carry
/// before devflow removes or uses it. A name is never a claim, and the project
/// label alone is not an identity either (every resource of a project has it),
/// so the service and the one branch, snapshot or operation the resource was
/// made for must agree too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownership {
    pub project: String,
    pub service: String,
    /// The label naming the one branch, snapshot or operation the resource
    /// was made for, with the value the catalog records.
    pub identity: (&'static str, String),
    /// A resource made before its identity label existed carries none (the
    /// volume of a snapshot taken by a pre-release build): accepted when the
    /// project and service agree, but never when the label is there with
    /// another value.
    pub identity_may_be_missing: bool,
}

/// The first label of a resource that disagrees with its [`Ownership`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelMismatch {
    pub label: &'static str,
    /// What the resource carries; `None` when it has no such label.
    pub found: Option<String>,
    pub expected: String,
}

impl std::fmt::Display for LabelMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.found {
            Some(found) => write!(
                f,
                "its `{}` label does not match the catalog (it says {found}, the catalog says {})",
                self.label, self.expected
            ),
            None => write!(
                f,
                "its `{}` label does not match the catalog (it has none, the catalog says {})",
                self.label, self.expected
            ),
        }
    }
}

impl Ownership {
    /// `None` when `labels` agree with the catalog.
    pub fn mismatch(&self, labels: &BTreeMap<String, String>) -> Option<LabelMismatch> {
        let (identity_key, identity_value) = &self.identity;
        let expected = [
            (LABEL_PROJECT, self.project.as_str()),
            (LABEL_SERVICE, self.service.as_str()),
            (*identity_key, identity_value.as_str()),
        ];
        for (label, value) in expected {
            let found = labels.get(label);
            if found.map(String::as_str) == Some(value) {
                continue;
            }
            if label == *identity_key && found.is_none() && self.identity_may_be_missing {
                // The exception is for a resource that predates the identity
                // label, not for one that carries another identity: a branch
                // or helper label says whose it is.
                if let Some(other) = [LABEL_BRANCH, LABEL_HELPER]
                    .into_iter()
                    .find(|other| other != identity_key && labels.contains_key(*other))
                {
                    return Some(LabelMismatch {
                        label: other,
                        found: labels.get(other).cloned(),
                        expected: format!(
                            "it was made for a `{identity_key}`, so it has no `{other}`"
                        ),
                    });
                }
                continue;
            }
            return Some(LabelMismatch {
                label,
                found: found.cloned(),
                expected: value.to_string(),
            });
        }
        None
    }

    /// Whether `mismatch` is the identity label being absent: the project and
    /// the service agree (they are checked first) and the resource simply
    /// predates the label.
    pub fn lacks_identity(&self, mismatch: &LabelMismatch) -> bool {
        mismatch.label == self.identity.0 && mismatch.found.is_none()
    }
}

/// A resource the catalog names that does not carry the catalog's labels:
/// kept, never removed. A typed error so that a caller can tell it from a
/// Docker failure.
#[derive(Debug)]
pub struct NotOwned {
    /// `volume NAME` or `container NAME`
    pub resource: String,
    pub mismatch: String,
}

impl std::fmt::Display for NotOwned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is recorded in the catalog but {}; devflow keeps it and does not remove it",
            self.resource, self.mismatch
        )
    }
}

impl std::error::Error for NotOwned {}

/// What [`remove_owned_volume`] and [`remove_owned_container`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Removed,
    /// Docker has no such resource: nothing to remove.
    Absent,
}

/// How long `docker pull` may take.
const PULL_BOUND: Duration = Duration::from_secs(15 * 60);

thread_local! {
    /// The end of the time the current thread's caller allows its Docker
    /// calls, set by [`within_deadline`].
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Run `f` with every Docker call it makes (on this thread) bounded by what is
/// left of `budget`, besides its own bound: a call that would start after the
/// time is used up is not made, and a running one is ended when it passes. A
/// nested scope never extends the outer one. This is how a recovery passes its
/// remaining deadline to every query, start, removal and log it needs.
pub fn within_deadline<T>(budget: Duration, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.with(|deadline| deadline.set(self.0));
        }
    }
    let previous = DEADLINE.with(Cell::get);
    let end = Instant::now() + budget;
    DEADLINE.with(|deadline| deadline.set(Some(previous.map_or(end, |p| p.min(end)))));
    let _restore = Restore(previous);
    f()
}

/// The bound of a call whose own bound is `per_call`: that, or what is left of
/// the enclosing [`within_deadline`], whichever is shorter. A [`QueryTimedOut`]
/// once nothing is left.
fn bound_of(what: &str, per_call: Duration) -> Result<Duration> {
    match DEADLINE.with(Cell::get) {
        None => Ok(per_call),
        Some(end) => {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(QueryTimedOut {
                    query: what.to_string(),
                    secs: 0,
                }
                .into());
            }
            Ok(per_call.min(left))
        }
    }
}

/// The words of `args` that name the command (`volume rm`, `stop`), for
/// messages: never an option or its value.
fn command_name(args: &[&str]) -> String {
    args.iter()
        .take_while(|a| !a.starts_with('-'))
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The one place devflow starts the `docker` client for the container
/// services: `docker args…` under `per_call` (and the enclosing
/// [`within_deadline`]) and `max_output`, in its own process group so that a
/// timeout ends everything it started. A timeout is a [`QueryTimedOut`]; any
/// other exit status is the caller's to read.
fn launch(
    args: &[&str],
    input: Option<&[u8]>,
    per_call: Duration,
    max_output: usize,
) -> Result<crate::subprocess::Output> {
    let what = command_name(args);
    let within = bound_of(&what, per_call)?;
    let mut command = Command::new("docker");
    command.args(args);
    let limits = crate::subprocess::Limits {
        max_output,
        ..crate::subprocess::Limits::new(within)
    };
    let out = crate::subprocess::run_bounded(command, input, limits)
        .context("cannot run `docker` (is Docker installed and running?)")?;
    if out.timed_out {
        return Err(QueryTimedOut {
            query: what,
            secs: within.as_secs().max(1),
        }
        .into());
    }
    Ok(out)
}

/// [`launch`] under the default bound of a query ([`query_bound`]).
fn quick(args: &[&str]) -> Result<crate::subprocess::Output> {
    launch(
        args,
        None,
        query_bound(),
        crate::subprocess::DEFAULT_MAX_OUTPUT,
    )
}

/// Run `docker args…`; success gives the trimmed standard output, failure an
/// error with the (redacted) standard error. Bounded: a pull by
/// [`PULL_BOUND`], anything else by [`query_bound`].
pub fn run(args: &[&str]) -> Result<String> {
    let per_call = if args.first() == Some(&"pull") {
        PULL_BOUND
    } else {
        query_bound()
    };
    run_within(args, per_call)
}

/// [`run`] under `per_call`.
pub fn run_within(args: &[&str], per_call: Duration) -> Result<String> {
    let out = launch(args, None, per_call, crate::subprocess::DEFAULT_MAX_OUTPUT)?;
    if !out.ok {
        bail!(
            "`docker {}` failed: {}",
            command_name(args),
            crate::redact::text(out.stderr.trim())
        );
    }
    anyhow::ensure!(
        !out.truncated,
        "`docker {}` printed more than devflow reads",
        command_name(args)
    );
    Ok(out.stdout.trim().to_string())
}

/// The identity of the Docker daemon the client talks to (`docker info`'s
/// ID): stable across restarts, different for another engine or machine.
pub fn daemon_id() -> Result<String> {
    let id = run(&["info", "--format", "{{.ID}}"])
        .context("cannot reach the Docker daemon (`docker info` failed)")?;
    anyhow::ensure!(
        !id.is_empty() && id != "<no value>",
        "the Docker daemon reports no ID; devflow cannot tell it apart from another engine"
    );
    Ok(id)
}

/// The name of the Docker context in use, for display.
pub fn context_name() -> Option<String> {
    if let Ok(name) = std::env::var("DOCKER_CONTEXT")
        && !name.is_empty()
    {
        return Some(name);
    }
    run(&["context", "show"]).ok().filter(|n| !n.is_empty())
}

/// An image reference to create containers from: by digest when the image
/// has one, by image id otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePin {
    /// `repository@sha256:…` or `sha256:…`; what `docker create` is given.
    pub reference: String,
    /// The image id, to tell apart images that share a tag.
    pub id: String,
}

/// Pin `image` (a tag or digest reference) to the content the daemon has
/// now, pulling it first if it is not there. The result does not move when
/// the tag does.
pub fn pin_image(image: &str) -> Result<ImagePin> {
    let inspect = || -> Result<Option<Value>> {
        let out = quick(&["image", "inspect", image])?;
        if !out.ok {
            return Ok(None);
        }
        let parsed: Value = serde_json::from_str(&out.stdout).context("unreadable image data")?;
        Ok(parsed.get(0).cloned())
    };
    let found = match inspect()? {
        Some(found) => found,
        None => {
            run(&["pull", "--quiet", image]).with_context(|| format!("cannot pull {image}"))?;
            inspect()?.with_context(|| format!("image {image} is not available after pulling"))?
        }
    };
    let id = found["Id"]
        .as_str()
        .with_context(|| format!("image {image} has no id"))?
        .to_string();
    let digests: Vec<&str> = found["RepoDigests"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Ok(ImagePin {
        reference: choose_reference(image, &digests, &id),
        id,
    })
}

/// The reference containers are created from: the image itself when it is
/// already a digest, the repository digest matching the image's repository,
/// else the image id.
fn choose_reference(image: &str, repo_digests: &[&str], id: &str) -> String {
    if image.contains("@sha256:") {
        return image.to_string();
    }
    let repository = repository_of(image);
    repo_digests
        .iter()
        .find(|digest| digest.split_once('@').map(|(repo, _)| repo) == Some(repository.as_str()))
        .or_else(|| repo_digests.first())
        .map_or_else(|| id.to_string(), |digest| (*digest).to_string())
}

/// `valkey/valkey:8-alpine` → `valkey/valkey`; `redis:8` → `redis`;
/// `localhost:5000/x:1` → `localhost:5000/x`. Docker prints the repository of
/// a Docker Hub library image as `redis`, not `docker.io/library/redis`.
fn repository_of(image: &str) -> String {
    let last_slash = image.rfind('/').map_or(0, |i| i + 1);
    let name_end = image[last_slash..]
        .find(':')
        .map_or(image.len(), |i| last_slash + i);
    let repository = &image[..name_end];
    repository
        .strip_prefix("docker.io/library/")
        .or_else(|| repository.strip_prefix("docker.io/"))
        .unwrap_or(repository)
        .to_string()
}

/// What the catalog says a container should be like; see [`create_container`].
#[derive(Debug, Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// The named volume.
    pub volume: String,
    /// Where the volume is mounted in the container (`/data`, `/var/lib/mysql`).
    pub data_dir: String,
    /// Host port on 127.0.0.1; `None` lets Docker choose (staging containers).
    pub host_port: Option<u16>,
    pub container_port: u16,
    /// No network at all and nothing published: a bootstrap that must not
    /// listen anywhere while its identity is being set.
    pub isolated: bool,
    /// Host files mounted read-only: `(host path, container path)`.
    pub files: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
    pub command: Vec<String>,
}

/// The `docker create` arguments for `spec`. Pure, so tests can check them.
pub fn create_args(spec: &ContainerSpec) -> Vec<String> {
    let mut args: Vec<String> = vec!["create".into(), "--name".into(), spec.name.clone()];
    // Containers start on setup, switch or `devflow service up`; devflow does
    // not bring a project's branches back on its own after a restart.
    args.extend(["--restart".into(), "no".into()]);
    for (key, value) in &spec.labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.extend(test_labels());
    if spec.isolated {
        args.extend(["--network".into(), "none".into()]);
    } else {
        let publish = match spec.host_port {
            Some(port) => format!("127.0.0.1:{port}:{}", spec.container_port),
            None => format!("127.0.0.1::{}", spec.container_port),
        };
        args.extend(["-p".into(), publish]);
    }
    args.extend(["-v".into(), format!("{}:{}", spec.volume, spec.data_dir)]);
    for (host, container) in &spec.files {
        // `--mount` refuses a source that does not exist instead of creating
        // a directory in its place, as `-v` would.
        args.extend([
            "--mount".into(),
            format!("type=bind,source={host},target={container},readonly"),
        ]);
    }
    args.push(spec.image.clone());
    args.extend(spec.command.iter().cloned());
    args
}

/// `--label dev.devflow.test=…` when `DEVFLOW_TEST_LABEL` is set.
pub fn test_labels() -> Vec<String> {
    match std::env::var("DEVFLOW_TEST_LABEL") {
        Ok(value) if !value.is_empty() => vec!["--label".into(), format!("{LABEL_TEST}={value}")],
        _ => Vec::new(),
    }
}

/// Create (not start) the container; returns its full id.
pub fn create_container(spec: &ContainerSpec) -> Result<String> {
    let args = create_args(spec);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let id = run(&refs).with_context(|| format!("cannot create container {}", spec.name))?;
    anyhow::ensure!(id.len() >= 12, "docker create returned no container id");
    Ok(id)
}

/// Why a container did not start.
#[derive(Debug)]
pub enum StartError {
    /// Docker could not publish the host port: something else holds it.
    Bind(String),
    Other(anyhow::Error),
}

pub fn start_container(id: &str) -> std::result::Result<(), StartError> {
    let out = quick(&["start", id]).map_err(StartError::Other)?;
    if out.ok {
        return Ok(());
    }
    let stderr = out.stderr;
    if crate::db::is_bind_failure(&stderr) {
        return Err(StartError::Bind(crate::db::summarize_bind_failure(&stderr)));
    }
    Err(StartError::Other(anyhow::anyhow!(
        "`docker start` failed: {}",
        crate::redact::text(stderr.trim())
    )))
}

/// Stop gracefully (SIGTERM, then SIGKILL after `timeout`). The command
/// itself is bounded too: a daemon that never answers is an error a minute
/// after the container's own deadline, not a hang.
pub fn stop_container(id: &str, timeout: Duration) -> Result<()> {
    let secs = timeout.as_secs().to_string();
    let out = launch(
        &["stop", "-t", &secs, id],
        None,
        timeout + Duration::from_secs(60),
        crate::subprocess::DEFAULT_MAX_OUTPUT,
    )?;
    if !out.ok {
        bail!(
            "`docker stop` failed: {}",
            crate::redact::text(out.message.as_deref().unwrap_or("no output"))
        );
    }
    Ok(())
}

/// A `docker stop` client process that is running for a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopClient {
    pub pid: u32,
    pub pgid: u32,
}

/// The `docker stop … CONTAINER` clients in `ps` output (`pid pgid command`
/// per line): a command line that runs `docker` (or a wrapper of the same
/// name) with the word `stop` and ends with the full container id, as
/// [`stop_container`] writes it. Pure, so that tests can check it.
pub fn parse_stop_clients(ps: &str, container: &str) -> Vec<StopClient> {
    ps.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let pgid: u32 = fields.next()?.parse().ok()?;
            let command: Vec<&str> = fields.collect();
            let runs_docker = command
                .iter()
                .any(|word| word.rsplit('/').next() == Some("docker"));
            (runs_docker && command.contains(&"stop") && command.last() == Some(&container))
                .then_some(StopClient { pid, pgid })
        })
        .collect()
}

/// The processes that are running now: `pid pgid command` per line.
fn process_list() -> Result<String> {
    let mut command = Command::new("ps");
    command.args(["-axww", "-o", "pid=,pgid=,command="]);
    let out = crate::subprocess::run(command, Duration::from_secs(15))
        .context("cannot run `ps` to look for a `docker` client still on its way")?;
    anyhow::ensure!(
        out.ok && !out.truncated,
        "`ps` could not list the processes: {}",
        out.message.as_deref().unwrap_or("no output")
    );
    Ok(out.stdout)
}

/// The `docker stop` clients that are running now for `container`.
pub fn stop_clients(container: &str) -> Result<Vec<StopClient>> {
    Ok(parse_stop_clients(&process_list()?, container))
}

/// The `docker create … --name NAME …` clients in `ps` output (see
/// [`parse_stop_clients`]): a client that was asked for a container of that
/// name and may not have reached the daemon yet. Pure.
pub fn parse_create_clients(ps: &str, name: &str) -> Vec<StopClient> {
    ps.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: u32 = fields.next()?.parse().ok()?;
            let pgid: u32 = fields.next()?.parse().ok()?;
            let command: Vec<&str> = fields.collect();
            let runs_docker = command
                .iter()
                .any(|word| word.rsplit('/').next() == Some("docker"));
            let named = command
                .windows(2)
                .any(|pair| pair[0] == "--name" && pair[1] == name);
            (runs_docker && command.contains(&"create") && named)
                .then_some(StopClient { pid, pgid })
        })
        .collect()
}

/// The `docker create` clients that are running now for a container `name`.
pub fn create_clients(name: &str) -> Result<Vec<StopClient>> {
    Ok(parse_create_clients(&process_list()?, name))
}

/// End `clients` (SIGKILL; their whole group when they lead one, so a wrapper's
/// children go too).
pub fn kill_stop_clients(clients: &[StopClient]) {
    for client in clients {
        let target = if client.pgid == client.pid {
            -(client.pid as i32)
        } else {
            client.pid as i32
        };
        // SAFETY: kill(2) with a pid or a negative process group id only
        // delivers a signal.
        unsafe { libc::kill(target, libc::SIGKILL) };
    }
}

/// `docker rm -f`: kills a running container. A container that is already
/// gone is not an error.
pub fn remove_container(id: &str) -> Result<()> {
    remove_container_within(id, query_bound())
}

/// [`remove_container`] under `within`.
pub fn remove_container_within(id: &str, within: Duration) -> Result<()> {
    let out = launch(
        &["rm", "--force", id],
        None,
        within,
        crate::subprocess::DEFAULT_MAX_OUTPUT,
    )?;
    if out.ok {
        return Ok(());
    }
    if out.stderr.to_lowercase().contains("no such container") {
        return Ok(());
    }
    bail!(
        "`docker rm` failed: {}",
        crate::redact::text(out.stderr.trim())
    )
}

/// What `docker container inspect` says.
#[derive(Debug, Clone)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub status: String,
    pub exit_code: i64,
    /// When the container last started (RFC 3339, the daemon's clock): the
    /// logs since then are those of its last run.
    pub started_at: String,
    /// When it last stopped (RFC 3339); `0001-01-01T00:00:00Z` before it ever did.
    pub finished_at: String,
    pub labels: BTreeMap<String, String>,
    pub image_id: String,
    /// The host port published for the container port, if any.
    pub published: Option<u16>,
    /// The host port the container was created to publish, running or not.
    pub configured: Option<u16>,
}

/// How long one query of the daemon (`inspect`, `logs`) may take before it is
/// ended: a daemon that does not answer is an error, not a hang. Tests may
/// shorten it with `DEVFLOW_TEST_QUERY_TIMEOUT_SECS`.
pub fn query_bound() -> Duration {
    std::env::var("DEVFLOW_TEST_QUERY_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|secs| *secs > 0)
        .map_or(Duration::from_secs(60), Duration::from_secs)
}

/// A query of the daemon that did not answer in time. A typed error so that a
/// caller that keeps its journal for the next attempt can tell it from an
/// answer.
#[derive(Debug)]
pub struct QueryTimedOut {
    pub query: String,
    pub secs: u64,
}

impl std::fmt::Display for QueryTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.secs == 0 {
            write!(
                f,
                "`docker {}` was not run: the time allowed for the Docker calls of this \
                 recovery is used up, so the state of the resource is unknown",
                self.query
            )
        } else {
            write!(
                f,
                "`docker {}` did not answer within {}; it was ended and the state of \
                 the resource is unknown",
                self.query,
                crate::text::counted(self.secs, "second")
            )
        }
    }
}

impl std::error::Error for QueryTimedOut {}

/// Whether `error` (or a cause) is a [`QueryTimedOut`]: the state of a resource
/// could not be read, which is never an answer.
pub fn timed_out(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<QueryTimedOut>().is_some())
}

/// [`launch`] under `within` and `max_output`.
fn query(args: &[&str], within: Duration, max_output: usize) -> Result<crate::subprocess::Output> {
    launch(args, None, within, max_output)
}

/// Inspect by id or name; `None` when there is no such container. Bounded by
/// [`query_bound`].
pub fn inspect_container(reference: &str) -> Result<Option<ContainerInfo>> {
    inspect_container_within(reference, query_bound())
}

/// [`inspect_container`] under `within`, for a caller that spends a budget.
pub fn inspect_container_within(
    reference: &str,
    within: Duration,
) -> Result<Option<ContainerInfo>> {
    let out = query(
        &["container", "inspect", reference],
        within,
        crate::subprocess::DEFAULT_MAX_OUTPUT,
    )?;
    if !out.ok {
        if out.stderr.to_lowercase().contains("no such") {
            return Ok(None);
        }
        bail!(
            "`docker container inspect` failed: {}",
            crate::redact::text(out.stderr.trim())
        );
    }
    anyhow::ensure!(
        !out.truncated,
        "`docker container inspect` printed more than devflow reads"
    );
    let parsed: Value = serde_json::from_str(&out.stdout).context("unreadable container data")?;
    Ok(parsed.get(0).map(parse_container))
}

fn parse_container(value: &Value) -> ContainerInfo {
    let labels = value["Config"]["Labels"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let published = value["NetworkSettings"]["Ports"]
        .as_object()
        .and_then(|ports| ports.values().find_map(|binding| binding.get(0)))
        .and_then(|binding| binding["HostPort"].as_str())
        .and_then(|port| port.parse().ok());
    let configured = value["HostConfig"]["PortBindings"]
        .as_object()
        .and_then(|ports| ports.values().find_map(|binding| binding.get(0)))
        .and_then(|binding| binding["HostPort"].as_str())
        .and_then(|port| port.parse().ok());
    ContainerInfo {
        id: value["Id"].as_str().unwrap_or_default().to_string(),
        name: value["Name"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string(),
        running: value["State"]["Running"].as_bool().unwrap_or(false),
        status: value["State"]["Status"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        exit_code: value["State"]["ExitCode"].as_i64().unwrap_or(0),
        started_at: value["State"]["StartedAt"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        finished_at: value["State"]["FinishedAt"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        labels,
        image_id: value["Image"].as_str().unwrap_or_default().to_string(),
        published,
        configured,
    }
}

/// The last lines a container logged (for failure messages), redacted.
pub fn logs_tail(id: &str, lines: u32) -> String {
    let lines = lines.to_string();
    // Bounded like every call, and only a diagnosis: a log that cannot be read
    // in time is left out of the message.
    match quick(&["logs", "--tail", &lines, id]) {
        Ok(out) => {
            let mut text = out.stdout;
            text.push_str(&out.stderr);
            crate::redact::text(text.trim())
        }
        Err(_) => String::new(),
    }
}

/// The most of a container's log that is read for its markers.
const LOGS_MAX: usize = 32 << 20;

/// Everything a container logged (both streams) since `since`, an RFC 3339
/// time on the daemon's clock such as [`ContainerInfo::started_at`]. Not
/// redacted: the caller looks for markers and does not show it. Bounded by
/// [`query_bound`]; a log too long to read is an error, never a prefix that
/// would hide its end.
pub fn logs_since(id: &str, since: &str) -> Result<String> {
    logs_since_within(id, since, query_bound())
}

/// [`logs_since`] under `within`, for a caller that spends a budget.
pub fn logs_since_within(id: &str, since: &str, within: Duration) -> Result<String> {
    let out = query(&["logs", "--since", since, id], within, LOGS_MAX)?;
    anyhow::ensure!(
        out.ok,
        "`docker logs` failed: {}",
        crate::redact::text(out.stderr.trim())
    );
    anyhow::ensure!(
        !out.truncated,
        "the log of container {id} is longer than {} MiB, which devflow does not read",
        LOGS_MAX >> 20
    );
    let mut text = out.stdout;
    text.push_str(&out.stderr);
    Ok(text)
}

/// What a command run inside a container printed and returned.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// `docker exec -i CONTAINER ARGS…` with `stdin` written to the command's
/// standard input, bounded by `timeout` (the client and what it started are
/// ended and reaped when it passes). Secrets belong in `stdin`: unlike an
/// argument or an `-e` variable it appears in no process list.
pub fn exec_stdin(
    container: &str,
    args: &[&str],
    stdin: &str,
    timeout: Duration,
) -> Result<ExecOutput> {
    let mut full: Vec<&str> = vec!["exec", "-i", container];
    full.extend_from_slice(args);
    let out = launch(
        &full,
        Some(stdin.as_bytes()),
        timeout,
        crate::subprocess::DEFAULT_MAX_OUTPUT,
    )?;
    anyhow::ensure!(
        !out.truncated,
        "`docker exec` in {container} printed more than {} MiB, which devflow does not read",
        crate::subprocess::DEFAULT_MAX_OUTPUT >> 20
    );
    Ok(ExecOutput {
        code: out.code.unwrap_or(-1),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// What `docker volume inspect` says.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

pub fn inspect_volume(name: &str) -> Result<Option<VolumeInfo>> {
    let out = quick(&["volume", "inspect", name])?;
    if !out.ok {
        if out.stderr.to_lowercase().contains("no such") {
            return Ok(None);
        }
        bail!(
            "`docker volume inspect` failed: {}",
            crate::redact::text(out.stderr.trim())
        );
    }
    let parsed: Value = serde_json::from_str(&out.stdout).context("unreadable volume data")?;
    Ok(parsed.get(0).map(|value| VolumeInfo {
        name: value["Name"].as_str().unwrap_or_default().to_string(),
        labels: value["Labels"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default(),
    }))
}

/// Create a named volume with `labels`. An existing volume of that name is
/// left alone (the caller checks its labels).
pub fn create_volume(name: &str, labels: &[(String, String)]) -> Result<()> {
    let mut args: Vec<String> = vec!["volume".into(), "create".into()];
    for (key, value) in labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.extend(test_labels());
    args.push(name.to_string());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&refs).map(|_| ())
}

/// A volume Docker refuses to remove because a container mounts it. A typed
/// error so that a caller that can find and remove the container retries.
#[derive(Debug)]
pub struct VolumeInUse {
    pub volume: String,
    pub detail: String,
}

impl std::fmt::Display for VolumeInUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`docker volume rm` failed: volume {} is in use: {}",
            self.volume, self.detail
        )
    }
}

impl std::error::Error for VolumeInUse {}

/// Remove a volume. One that is already gone is not an error; one a container
/// mounts is a [`VolumeInUse`].
pub fn remove_volume(name: &str) -> Result<()> {
    let out = quick(&["volume", "rm", name])?;
    if out.ok {
        return Ok(());
    }
    let stderr = out.stderr.to_lowercase();
    if stderr.contains("no such volume") {
        return Ok(());
    }
    if stderr.contains("volume is in use") {
        return Err(VolumeInUse {
            volume: name.to_string(),
            detail: crate::redact::text(out.stderr.trim()),
        }
        .into());
    }
    bail!(
        "`docker volume rm` failed: {}",
        crate::redact::text(out.stderr.trim())
    )
}

/// Remove the volume `name` only if it carries the labels of `expected`: the
/// project, the service and the branch or snapshot the catalog records it for.
/// A volume that merely has the name, or has the project's label of another
/// service or owner, is a [`NotOwned`] error and stays; one that is already
/// gone is [`Removal::Absent`].
pub fn remove_owned_volume(name: &str, expected: &Ownership) -> Result<Removal> {
    let Some(volume) = inspect_volume(name)? else {
        return Ok(Removal::Absent);
    };
    if let Some(mismatch) = expected.mismatch(&volume.labels) {
        return Err(NotOwned {
            resource: format!("volume {name}"),
            mismatch: mismatch.to_string(),
        }
        .into());
    }
    remove_volume(name)?;
    Ok(Removal::Removed)
}

/// Kill and remove the container `reference` (an id or a name) only if it
/// carries the labels of `expected` and, when `recorded_id` is given, is the
/// container with that id. It is confirmed gone before this returns.
pub fn remove_owned_container(
    reference: &str,
    recorded_id: Option<&str>,
    expected: &Ownership,
) -> Result<Removal> {
    let Some(info) = inspect_container(reference)? else {
        return Ok(Removal::Absent);
    };
    if let Some(id) = recorded_id
        && info.id != id
    {
        return Err(NotOwned {
            resource: format!("container {}", info.name),
            mismatch: "it no longer has the recorded id".into(),
        }
        .into());
    }
    if let Some(mismatch) = expected.mismatch(&info.labels) {
        return Err(NotOwned {
            resource: format!("container {}", info.name),
            mismatch: mismatch.to_string(),
        }
        .into());
    }
    kill_and_confirm(&info.id, Duration::from_secs(20))?;
    Ok(Removal::Removed)
}

/// A short-lived container that runs one script with volumes mounted: the
/// way files are cloned inside the Docker VM, where the filesystem that can
/// share blocks lives.
#[derive(Debug, Clone)]
pub struct HelperSpec {
    pub name: String,
    pub image: String,
    /// `(volume, mount point, read only)`
    pub mounts: Vec<(String, String, bool)>,
    pub env: Vec<(String, String)>,
    pub labels: Vec<(String, String)>,
    pub script: String,
}

/// `docker create` arguments of a helper. Pure.
pub fn helper_args(spec: &HelperSpec) -> Vec<String> {
    let mut args: Vec<String> = vec!["create".into(), "--name".into(), spec.name.clone()];
    args.extend(["--network".into(), "none".into()]);
    for (key, value) in &spec.labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.extend(test_labels());
    for (volume, path, read_only) in &spec.mounts {
        let mode = if *read_only { ":ro" } else { "" };
        args.extend(["-v".into(), format!("{volume}:{path}{mode}")]);
    }
    for (key, value) in &spec.env {
        args.extend(["-e".into(), format!("{key}={value}")]);
    }
    args.push(spec.image.clone());
    args.extend(["sh".into(), "-c".into(), spec.script.clone()]);
    args
}

pub fn create_helper(spec: &HelperSpec) -> Result<String> {
    let args = helper_args(spec);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&refs).with_context(|| format!("cannot create the copy helper {}", spec.name))
}

/// `docker create` arguments of a tombstone: a container that holds `name`
/// and nothing else. It is made from an image that is already present (never
/// pulled), has no mounts, no network and no published port, and is never
/// started (if someone started it, it would run `true` and exit). Pure.
pub fn tombstone_args(name: &str, labels: &[(String, String)], image: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "create".into(),
        "--name".into(),
        name.to_string(),
        "--pull".into(),
        "never".into(),
        "--restart".into(),
        "no".into(),
        "--network".into(),
        "none".into(),
    ];
    for (key, value) in labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    args.extend(test_labels());
    args.extend([image.to_string(), "true".into()]);
    args
}

/// What creating a tombstone came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TombstoneCreate {
    /// The container now holds the name; its id.
    Created(String),
    /// Docker refused: a container of that name exists (the real helper, or an
    /// earlier tombstone).
    NameInUse,
}

/// Whether Docker's refusal to create a container says its name is taken.
fn name_conflict(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    stderr.contains("is already in use") && stderr.contains("container name")
}

/// Create (never start) a tombstone called `name`. A create of that name that
/// reaches the daemon afterwards fails with a name conflict, which is the
/// positive evidence that a helper attempt of unknown outcome can no longer
/// materialize.
pub fn create_tombstone(
    name: &str,
    labels: &[(String, String)],
    image: &str,
) -> Result<TombstoneCreate> {
    let args = tombstone_args(name, labels, image);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = quick(&refs)?;
    if out.ok {
        let id = out.stdout.trim().to_string();
        anyhow::ensure!(id.len() >= 12, "docker create returned no container id");
        return Ok(TombstoneCreate::Created(id));
    }
    if name_conflict(&out.stderr) {
        return Ok(TombstoneCreate::NameInUse);
    }
    bail!(
        "`docker create` of the tombstone {name} failed: {}",
        crate::redact::text(out.stderr.trim())
    )
}

/// How a helper ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperOutcome {
    Exited { code: i64, stdout: String },
    TimedOut,
}

/// Start the helper `id` and wait for it until `deadline`. On the deadline
/// the wait is abandoned and [`HelperOutcome::TimedOut`] returned; the
/// caller kills the helper and confirms it is gone.
pub fn run_helper(id: &str, deadline: Instant) -> Result<HelperOutcome> {
    run(&["start", id]).context("cannot start the copy helper")?;
    // The wait is bounded by the helper's own deadline (and by the enclosing
    // `within_deadline`, which is an error and not a timed-out copy).
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    let waited = match launch(&["wait", id], None, left, 1 << 16) {
        Ok(out) => out,
        Err(error) if timed_out(&error) && Instant::now() >= deadline => {
            return Ok(HelperOutcome::TimedOut);
        }
        Err(error) => return Err(error),
    };
    let code: i64 = waited
        .stdout
        .trim()
        .parse()
        .with_context(|| "`docker wait` returned no exit code".to_string())?;
    let logs = quick(&["logs", id])?;
    Ok(HelperOutcome::Exited {
        code,
        stdout: format!("{}{}", logs.stdout, logs.stderr),
    })
}

/// Kill and remove container `id`, then wait until Docker no longer knows
/// it. Fails if it is still there after `within`.
/// `within` covers the removal as well as the wait: the timer starts before
/// the removal is asked for, and each call is bounded by what is left of it.
pub fn kill_and_confirm(id: &str, within: Duration) -> Result<()> {
    let start = Instant::now();
    let left = |floor: Duration| {
        within
            .saturating_sub(start.elapsed())
            .max(floor)
            .min(query_bound())
    };
    remove_container_within(id, left(Duration::from_secs(1)))?;
    loop {
        if inspect_container_within(id, left(Duration::from_secs(1)))?.is_none() {
            return Ok(());
        }
        anyhow::ensure!(
            start.elapsed() < within,
            "container {id} is still present after being removed"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The containers (running or not) that mount `volume`.
pub fn containers_mounting(volume: &str) -> Result<Vec<ContainerInfo>> {
    let filter = format!("volume={volume}");
    let ids = run(&["ps", "--all", "--quiet", "--no-trunc", "--filter", &filter])?;
    let mut found = Vec::new();
    for id in ids.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(info) = inspect_container(id)? {
            found.push(info);
        }
    }
    Ok(found)
}

/// Containers carrying the project label: `(id, name, labels)`; for
/// reporting resources the catalog does not know.
pub fn list_project_containers(project: &str) -> Result<Vec<ContainerInfo>> {
    let filter = format!("label={LABEL_PROJECT}={project}");
    let ids = run(&["ps", "--all", "--quiet", "--no-trunc", "--filter", &filter])?;
    let mut found = Vec::new();
    for id in ids.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(info) = inspect_container(id)? {
            found.push(info);
        }
    }
    Ok(found)
}

/// Volumes carrying the project label.
pub fn list_project_volumes(project: &str) -> Result<Vec<String>> {
    let filter = format!("label={LABEL_PROJECT}={project}");
    let names = run(&["volume", "ls", "--quiet", "--filter", &filter])?;
    Ok(names
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_bounds_every_call_made_inside_it_and_only_inside_it() {
        let long = Duration::from_secs(60);
        assert_eq!(bound_of("ps", long).unwrap(), long, "no scope, own bound");
        within_deadline(Duration::from_secs(10), || {
            let bound = bound_of("ps", long).unwrap();
            assert!(bound <= Duration::from_secs(10) && bound > Duration::from_secs(8));
            // A shorter own bound wins; a nested scope never extends the outer.
            assert_eq!(
                bound_of("ps", Duration::from_secs(2)).unwrap(),
                Duration::from_secs(2)
            );
            within_deadline(Duration::from_secs(3600), || {
                assert!(bound_of("ps", long).unwrap() <= Duration::from_secs(10));
            });
            within_deadline(Duration::from_secs(1), || {
                assert!(bound_of("ps", long).unwrap() <= Duration::from_secs(1));
            });
            assert!(bound_of("ps", long).unwrap() > Duration::from_secs(8));
        });
        assert_eq!(bound_of("ps", long).unwrap(), long, "put back afterwards");
    }

    #[test]
    fn a_call_after_the_deadline_is_not_made_and_is_a_typed_timeout() {
        within_deadline(Duration::ZERO, || {
            let error = quick(&["version"]).err().expect("not run");
            assert!(timed_out(&error), "{error:#}");
            let shown = format!("{error:#}");
            assert!(shown.contains("not run") && shown.contains("`docker version`"));
            // The same through a context, as callers add them.
            let wrapped = error.context("cannot read the volume");
            assert!(timed_out(&wrapped));
        });
        assert!(!timed_out(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn every_docker_call_of_the_services_goes_through_the_one_bounded_helper() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services");
        let spawn = format!("Command::new({:?})", "docker");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Tests (the bottom of a file) may run what they like.
            let code = text.split("#[cfg(test)]").next().unwrap();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            for (n, line) in code.lines().enumerate() {
                if line.contains(&spawn) {
                    found.push(format!("{name}:{}", n + 1));
                }
                if name == "docker.rs" {
                    for unbounded in [".output()", ".spawn()", ".status()"] {
                        assert!(
                            !line.contains(unbounded),
                            "docker.rs:{} runs a process without a bound: {line}",
                            n + 1
                        );
                    }
                }
            }
        }
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("docker.rs:"), "{found:?}");
    }

    #[test]
    fn repositories_are_read_from_references() {
        assert_eq!(repository_of("valkey/valkey:8-alpine"), "valkey/valkey");
        assert_eq!(repository_of("redis:8-alpine"), "redis");
        assert_eq!(repository_of("redis"), "redis");
        assert_eq!(repository_of("localhost:5000/x:1"), "localhost:5000/x");
        assert_eq!(repository_of("localhost:5000/x"), "localhost:5000/x");
        assert_eq!(repository_of("docker.io/library/redis:8"), "redis");
        assert_eq!(repository_of("ghcr.io/o/r:tag"), "ghcr.io/o/r");
    }

    #[test]
    fn the_pinned_reference_prefers_the_matching_repository_digest() {
        let digests = ["other/thing@sha256:aaa", "valkey/valkey@sha256:bbb"];
        assert_eq!(
            choose_reference("valkey/valkey:8-alpine", &digests, "sha256:id"),
            "valkey/valkey@sha256:bbb"
        );
        assert_eq!(
            choose_reference("valkey/valkey:8", &[], "sha256:id"),
            "sha256:id"
        );
        // Any digest beats none when the repository was renamed.
        assert_eq!(
            choose_reference("mirror/v:1", &digests, "sha256:id"),
            "other/thing@sha256:aaa"
        );
        assert_eq!(
            choose_reference("x@sha256:ccc", &digests, "sha256:id"),
            "x@sha256:ccc"
        );
    }

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "devflow-12345678-cache-abcdef012345-1".into(),
            image: "valkey/valkey@sha256:bbb".into(),
            volume: "devflow-12345678-cache-v1".into(),
            data_dir: "/data".into(),
            host_port: Some(23456),
            container_port: 6379,
            isolated: false,
            files: Vec::new(),
            labels: vec![(LABEL_PROJECT.into(), "p".into())],
            command: vec!["valkey-server".into(), "--appendonly".into(), "no".into()],
        }
    }

    #[test]
    fn an_isolated_container_has_no_network_and_publishes_nothing() {
        let mut bootstrap = spec();
        bootstrap.isolated = true;
        bootstrap.host_port = Some(23456);
        bootstrap.data_dir = "/var/lib/mysql".into();
        bootstrap.files = vec![(
            "/data dir/mysql-profile-v1.cnf".into(),
            "/etc/mysql/conf.d/devflow.cnf".into(),
        )];
        let args = create_args(&bootstrap);
        let joined = args.join(" ");
        assert!(joined.contains("--network none"), "{joined}");
        assert!(!args.contains(&"-p".to_string()), "{joined}");
        assert!(joined.contains("-v devflow-12345678-cache-v1:/var/lib/mysql"));
        // A file is mounted read-only, with `--mount` so that a missing
        // source is an error and not a directory made in its place.
        assert!(
            args.contains(
                &"type=bind,source=/data dir/mysql-profile-v1.cnf,\
                  target=/etc/mysql/conf.d/devflow.cnf,readonly"
                    .to_string()
            ),
            "{args:?}"
        );
    }

    #[test]
    fn containers_publish_on_loopback_and_never_restart_by_themselves() {
        let args = create_args(&spec());
        assert_eq!(args[0], "create");
        let joined = args.join(" ");
        assert!(joined.contains("-p 127.0.0.1:23456:6379"), "{joined}");
        assert!(joined.contains("--restart no"), "{joined}");
        assert!(joined.contains("-v devflow-12345678-cache-v1:/data"));
        assert!(joined.contains(&format!("--label {LABEL_PROJECT}=p")));
        // The image comes before the command.
        let image = args
            .iter()
            .position(|a| a == "valkey/valkey@sha256:bbb")
            .unwrap();
        assert_eq!(args[image + 1], "valkey-server");
        let mut ephemeral = spec();
        ephemeral.host_port = None;
        assert!(create_args(&ephemeral).contains(&"127.0.0.1::6379".to_string()));
    }

    #[test]
    fn helpers_have_no_network_and_mount_the_source_read_only() {
        let args = helper_args(&HelperSpec {
            name: "h".into(),
            image: HELPER_IMAGE.into(),
            mounts: vec![
                ("src".into(), "/src".into(), true),
                ("dst".into(), "/dst".into(), false),
            ],
            env: vec![("MODE".into(), "reflink".into())],
            labels: vec![(LABEL_HELPER.into(), "op".into())],
            script: "true".into(),
        });
        let joined = args.join(" ");
        assert!(joined.contains("--network none"));
        assert!(joined.contains("-v src:/src:ro"));
        assert!(joined.contains("-v dst:/dst "));
        assert!(joined.ends_with("sh -c true"));
    }

    #[test]
    fn a_tombstone_holds_a_name_and_nothing_else() {
        let args = tombstone_args(
            "devflow-p-cache-verify-1-2",
            &[(LABEL_TOMBSTONE.into(), "op".into())],
            HELPER_IMAGE,
        );
        let joined = args.join(" ");
        assert!(joined.starts_with("create --name devflow-p-cache-verify-1-2 "));
        // Never pulled, no network, no mounts or ports, and harmless if started.
        assert!(joined.contains("--pull never"), "{joined}");
        assert!(joined.contains("--network none"), "{joined}");
        assert!(
            !joined.contains(" -v ") && !joined.contains(" -p "),
            "{joined}"
        );
        assert!(joined.contains(&format!("--label {LABEL_TOMBSTONE}=op")));
        assert!(
            !joined.contains(LABEL_HELPER),
            "a tombstone is not a helper: {joined}"
        );
        assert!(
            joined.ends_with(&format!("{HELPER_IMAGE} true")),
            "{joined}"
        );
    }

    #[test]
    fn docker_says_a_container_name_is_taken_in_one_way() {
        assert!(name_conflict(
            "Error response from daemon: Conflict. The container name \"/x\" is already in use \
             by container \"abc\". You have to remove (or rename) that container to be able to \
             reuse that name."
        ));
        assert!(!name_conflict(
            "Error response from daemon: No such image: x"
        ));
        assert!(!name_conflict("volume is already in use"));
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn owned_by_branch(branch: &str) -> Ownership {
        Ownership {
            project: "p".into(),
            service: "cache".into(),
            identity: (LABEL_BRANCH, branch.into()),
            identity_may_be_missing: false,
        }
    }

    #[test]
    fn removal_needs_the_project_the_service_and_the_identity() {
        let expected = owned_by_branch("b1");
        let ours = labels(&[
            (LABEL_PROJECT, "p"),
            (LABEL_SERVICE, "cache"),
            (LABEL_BRANCH, "b1"),
            (LABEL_TEST, "run"),
        ]);
        assert_eq!(expected.mismatch(&ours), None);
        // The project's label alone is not enough: this is the namesake that
        // another branch of the same project (or another service) left.
        let other_branch = labels(&[
            (LABEL_PROJECT, "p"),
            (LABEL_SERVICE, "cache"),
            (LABEL_BRANCH, "b2"),
        ]);
        assert_eq!(
            expected.mismatch(&other_branch).unwrap().label,
            LABEL_BRANCH
        );
        let other_service = labels(&[
            (LABEL_PROJECT, "p"),
            (LABEL_SERVICE, "queue"),
            (LABEL_BRANCH, "b1"),
        ]);
        assert_eq!(
            expected.mismatch(&other_service).unwrap().label,
            LABEL_SERVICE
        );
        let other_project = labels(&[
            (LABEL_PROJECT, "q"),
            (LABEL_SERVICE, "cache"),
            (LABEL_BRANCH, "b1"),
        ]);
        assert_eq!(
            expected.mismatch(&other_project).unwrap().label,
            LABEL_PROJECT
        );
        let only_project = labels(&[(LABEL_PROJECT, "p")]);
        assert_eq!(
            expected.mismatch(&only_project).unwrap().label,
            LABEL_SERVICE
        );
        assert!(expected.mismatch(&labels(&[])).is_some());
        // A snapshot label is not a branch label.
        let snapshot = labels(&[
            (LABEL_PROJECT, "p"),
            (LABEL_SERVICE, "cache"),
            (LABEL_SNAPSHOT, "b1"),
        ]);
        assert_eq!(expected.mismatch(&snapshot).unwrap().found, None);
    }

    #[test]
    fn a_resource_of_a_pre_release_build_may_lack_only_the_identity_label() {
        let expected = Ownership {
            project: "p".into(),
            service: "cache".into(),
            identity: (LABEL_SNAPSHOT, "s1".into()),
            identity_may_be_missing: true,
        };
        let legacy = labels(&[(LABEL_PROJECT, "p"), (LABEL_SERVICE, "cache")]);
        assert_eq!(expected.mismatch(&legacy), None);
        // The project and service still have to agree.
        let foreign = labels(&[(LABEL_PROJECT, "p"), (LABEL_SERVICE, "queue")]);
        assert_eq!(expected.mismatch(&foreign).unwrap().label, LABEL_SERVICE);
        assert!(
            expected
                .mismatch(&labels(&[(LABEL_PROJECT, "p")]))
                .is_some()
        );
        // A label that is there with another value is another snapshot's.
        let other = labels(&[
            (LABEL_PROJECT, "p"),
            (LABEL_SERVICE, "cache"),
            (LABEL_SNAPSHOT, "s2"),
        ]);
        let mismatch = expected.mismatch(&other).unwrap();
        assert_eq!(mismatch.label, LABEL_SNAPSHOT);
        assert!(!expected.lacks_identity(&mismatch));
        // Strictly, the missing label is the mismatch, and it is recognised.
        let strict = Ownership {
            identity_may_be_missing: false,
            ..expected
        };
        let mismatch = strict.mismatch(&legacy).unwrap();
        assert!(strict.lacks_identity(&mismatch));
        assert!(mismatch.to_string().contains("label does not match"));
    }

    #[test]
    fn the_legacy_exception_never_covers_a_resource_with_another_identity() {
        let expected = Ownership {
            project: "p".into(),
            service: "cache".into(),
            identity: (LABEL_SNAPSHOT, "s1".into()),
            identity_may_be_missing: true,
        };
        // A volume of a branch, or the helper of an operation, with the right
        // project and service and no snapshot label is not a legacy snapshot.
        for (label, value) in [(LABEL_BRANCH, "b1"), (LABEL_HELPER, "op1")] {
            let namesake = labels(&[
                (LABEL_PROJECT, "p"),
                (LABEL_SERVICE, "cache"),
                (label, value),
            ]);
            let mismatch = expected.mismatch(&namesake).expect(label);
            assert_eq!(mismatch.label, label);
            assert_eq!(mismatch.found.as_deref(), Some(value));
            assert!(!expected.lacks_identity(&mismatch));
        }
    }

    #[test]
    fn docker_stop_clients_are_found_by_their_command_line() {
        let id = "a".repeat(64);
        let ps = format!(
            "  101   101 /usr/local/bin/docker stop -t 300 {id}\n\
               102   100 /bin/sh /tmp/shim/docker stop -t 0 {id}\n\
               103   103 docker stop -t 300 {}\n\
               104   104 docker logs --tail 5 {id}\n\
               105   105 docker exec -i {id} mysql stop\n\
               106   106 vim docker-stop-notes {id}\n\
               not a process line\n",
            "b".repeat(64)
        );
        assert_eq!(
            parse_stop_clients(&ps, &id),
            vec![
                StopClient {
                    pid: 101,
                    pgid: 101
                },
                StopClient {
                    pid: 102,
                    pgid: 100
                }
            ]
        );
        assert!(parse_stop_clients("", &id).is_empty());
    }

    #[test]
    fn docker_create_clients_are_found_by_the_name_they_create() {
        let ps = "  201   201 /usr/local/bin/docker create --name devflow-p-cache-verify-abc --network none x\n\
                    202   200 /bin/sh /tmp/shim/docker create --name devflow-p-cache-verify-abc -v v:/v y\n\
                    203   203 docker create --name devflow-p-cache-verify-other x\n\
                    204   204 docker run --name devflow-p-cache-verify-abc x\n\
                    205   205 vim create --name devflow-p-cache-verify-abc\n";
        let found = parse_create_clients(ps, "devflow-p-cache-verify-abc");
        assert_eq!(
            found.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![201, 202]
        );
        assert!(parse_create_clients("", "x").is_empty());
    }

    #[test]
    fn container_data_is_read_from_inspect_json() {
        let value: Value = serde_json::from_str(
            r#"{"Id":"abc","Name":"/devflow-x","Image":"sha256:img",
                "State":{"Running":true,"Status":"running","ExitCode":0,"StartedAt":"2026-10-11T10:00:00.123456789Z"},
                "Config":{"Labels":{"dev.devflow.branch":"b"}},
                "NetworkSettings":{"Ports":{"6379/tcp":[{"HostIp":"127.0.0.1","HostPort":"23456"}]}}}"#,
        )
        .unwrap();
        let info = parse_container(&value);
        assert_eq!(info.id, "abc");
        assert_eq!(info.name, "devflow-x");
        assert!(info.running);
        assert_eq!(info.published, Some(23456));
        assert_eq!(info.started_at, "2026-10-11T10:00:00.123456789Z");
        assert_eq!(info.labels["dev.devflow.branch"], "b");
        // A stopped container publishes nothing.
        let stopped: Value = serde_json::from_str(
            r#"{"Id":"abc","Name":"/x","State":{"Running":false,"Status":"exited","ExitCode":3},
                "Config":{"Labels":null},"NetworkSettings":{"Ports":{}}}"#,
        )
        .unwrap();
        let info = parse_container(&stopped);
        assert!(!info.running && info.published.is_none() && info.labels.is_empty());
        assert_eq!(info.exit_code, 3);
    }
}
