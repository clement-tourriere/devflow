//! Container services seen from workspaces: which branch a worktree is
//! bound to, what `--data-from` means for a container service, and the
//! pieces of removal, listing and orphan reports that read receipts.
//!
//! The pipeline (`setup`) builds on these; the CLI and the desktop app call
//! them through `setup`'s typed functions.
use super::Service;
use super::catalog::{RecordState, ServiceCatalog, SourceRef};
use crate::config::Engine;
use crate::db::DataSource;
use crate::error::{OwnershipUnknown, UnreadReceipt};
use crate::project::Project;
use crate::receipt::{CloneReport, Receipt, ServiceBinding};
use crate::setup;
use crate::worktree;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use uuid::Uuid;

/// The binding of container service `key` in `receipt`, if it is one of
/// `engine`.
pub fn binding<'a>(receipt: &'a Receipt, key: &str, engine: Engine) -> Option<&'a ServiceBinding> {
    receipt
        .services
        .get(key)
        .filter(|binding| binding.engine == engine && binding.branch_id.is_some())
}

/// What a `--data-from` request means for container service `svc`: the
/// identity it resolves to in the service's catalog and the label recorded
/// in the receipt. A missing source is an error naming the service, never an
/// empty database in its place.
pub fn resolve_source(
    project: &Project,
    svc: &Service,
    source: &DataSource,
) -> Result<(SourceRef, String)> {
    resolve_in(project, &svc.key, svc.engine, &svc.records()?, source)
}

/// [`resolve_source`] over the records of service `key` read from the catalog
/// file: no Docker, so a frontend can resolve what it is about to show.
pub fn resolve_in(
    project: &Project,
    key: &str,
    engine: Engine,
    records: &ServiceCatalog,
    source: &DataSource,
) -> Result<(SourceRef, String)> {
    Ok(match source {
        DataSource::Empty => (SourceRef::Empty, "empty".to_string()),
        DataSource::Workspace { name } => {
            let workspace = worktree::find(project, name)?.with_context(|| {
                format!("no workspace named '{name}' to take the data of '{key}' from")
            })?;
            let receipt = setup::receipt(project, &workspace)?;
            let id = receipt
                .as_ref()
                .and_then(|r| binding(r, key, engine))
                .and_then(|b| b.branch_id)
                .with_context(|| {
                    format!(
                        "workspace '{name}' has no branch of service '{key}' to clone; use \
                         --data-from empty or snapshot:NAME"
                    )
                })?;
            branch_ready(key, records, id)?;
            (SourceRef::Branch { id }, format!("workspace:{name}"))
        }
        DataSource::Branch { .. } | DataSource::BranchPrefix { .. } => {
            let id = source
                .branch_id(branch_ids(records))?
                .context("a branch source names a branch")?;
            branch_ready(key, records, id)?;
            (SourceRef::Branch { id }, format!("branch:{id}"))
        }
        DataSource::Snapshot { name, version } => {
            let version = version.map(|v| v as u32);
            let snapshot = records
                .find_snapshot(name, version)
                .with_context(|| match version {
                    Some(v) => format!("service '{key}' has no snapshot {name}@v{v}"),
                    None => format!("service '{key}' has no snapshot named '{name}'"),
                })?;
            (
                SourceRef::Snapshot { id: snapshot.id },
                format!("snapshot:{}@v{}", snapshot.name, snapshot.version),
            )
        }
    })
}

/// A resolved source as a receipt records it: `empty`, `branch:<id>` or
/// `snapshot:<id>` (the catalog's ids, not names).
pub fn encode_ref(source: SourceRef) -> String {
    match source {
        SourceRef::Empty => "empty".to_string(),
        SourceRef::Branch { id } => format!("branch:{id}"),
        SourceRef::Snapshot { id } => format!("snapshot:{id}"),
    }
}

/// The inverse of [`encode_ref`].
pub fn decode_ref(text: &str) -> Result<SourceRef> {
    if text == "empty" {
        return Ok(SourceRef::Empty);
    }
    let parse = |id: &str| {
        id.parse::<Uuid>()
            .with_context(|| format!("'{text}' is not a recorded data source"))
    };
    if let Some(id) = text.strip_prefix("branch:") {
        return Ok(SourceRef::Branch { id: parse(id)? });
    }
    if let Some(id) = text.strip_prefix("snapshot:") {
        return Ok(SourceRef::Snapshot { id: parse(id)? });
    }
    anyhow::bail!("'{text}' is not a recorded data source")
}

/// The source a receipt pinned is still in the catalog and usable. Nothing
/// is looked up by name: a branch or snapshot that is gone is an error,
/// never another one with the same name.
pub fn require_source(svc: &Service, source: SourceRef) -> Result<()> {
    match source {
        SourceRef::Empty => Ok(()),
        SourceRef::Branch { id } => ensure_branch_ready(svc, id),
        SourceRef::Snapshot { id } => {
            let snapshot = svc
                .snapshot(id)?
                .with_context(|| format!("service '{}' has no snapshot {id}", svc.key))?;
            ensure!(
                snapshot.state == RecordState::Ready,
                "snapshot {id} of service '{}' is not ready ({:?})",
                svc.key,
                snapshot.state
            );
            Ok(())
        }
    }
}

fn ensure_branch_ready(svc: &Service, id: Uuid) -> Result<()> {
    branch_ready(&svc.key, &svc.records()?, id)
}

/// The ids of the branches a listing shows: the ones a short id can name.
fn branch_ids(records: &ServiceCatalog) -> impl Iterator<Item = Uuid> + '_ {
    records
        .branches
        .iter()
        .filter(|b| b.state != RecordState::Aborted)
        .map(|b| b.id)
}

fn branch_ready(key: &str, records: &ServiceCatalog, id: Uuid) -> Result<()> {
    let branch = records
        .branch(id)
        .with_context(|| format!("service '{key}' has no branch {id}"))?;
    ensure!(
        branch.state == RecordState::Ready,
        "branch {id} of service '{key}' is not ready ({:?})",
        branch.state
    );
    Ok(())
}

/// The receipt binding for a ready branch: the URL with its password, the
/// container endpoint and how the data was cloned.
pub fn binding_of(
    svc: &Service,
    branch: &super::catalog::BranchRecord,
    source_label: String,
    applied: Option<String>,
) -> Result<ServiceBinding> {
    let endpoint = svc.endpoint(branch)?;
    Ok(ServiceBinding::container(
        svc.engine,
        branch.id,
        endpoint.url,
        endpoint.container_endpoint,
        source_label,
        CloneReport {
            method: branch.clone,
            captured_at: branch.captured_at,
            allocated_bytes: branch.allocated_bytes,
            source_downtime_ms: branch.downtime_ms,
        },
        applied,
    ))
}

/// What a recorded branch is, for the listings of orphans.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanBranch {
    pub service: String,
    pub branch: Uuid,
    pub name: String,
    pub owner: Option<Uuid>,
    /// Why it counts: no workspace holds the instance that owns it.
    pub reason: String,
}

/// Recorded data nothing configured or checked out uses any more.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Orphans {
    /// Services in the catalog that the configuration no longer has (their
    /// data is kept until `devflow service NAME forget`).
    pub services: Vec<String>,
    /// Branches of configured services whose owning workspace is gone. Empty
    /// while the scan is [`incomplete`](Self::incomplete): an unreadable
    /// receipt may hold the owner, so no branch is called an orphan then.
    pub branches: Vec<OrphanBranch>,
    /// Workspaces whose receipt could not be read (unreadable, malformed or
    /// written by a newer devflow). Who owns a branch is unknown while this
    /// is not empty.
    pub incomplete: Vec<UnreadReceipt>,
}

impl Orphans {
    /// Every workspace's receipt was read, so `branches` is the whole answer.
    pub fn is_complete(&self) -> bool {
        self.incomplete.is_empty()
    }
}

/// Compare the project's catalog with its configuration and worktrees.
///
/// A receipt that cannot be read is reported in [`Orphans::incomplete`] and no
/// branch is called an orphan meanwhile.
pub fn orphans(project: &Project) -> Result<Orphans> {
    orphans_in(project, &super::catalog::Store::path_for(project.id())?)
}

fn orphans_in(project: &Project, path: &std::path::Path) -> Result<Orphans> {
    let mut out = Orphans::default();
    let Some(catalog) = super::catalog::read_file(path)? else {
        return Ok(out);
    };
    // The instances that exist: every workspace's receipt. One that cannot be
    // read may hold any instance, so the scan is incomplete, not smaller.
    let mut instances: Vec<Uuid> = Vec::new();
    for workspace in worktree::list(project)? {
        match Receipt::load(&workspace.git_dir) {
            Ok(Some(receipt)) => instances.push(receipt.instance_id),
            Ok(None) => {}
            Err(error) => out.incomplete.push(UnreadReceipt {
                workspace: workspace.name,
                reason: format!("{error:#}"),
            }),
        }
    }
    for (key, service) in &catalog.services {
        let live = service
            .branches
            .iter()
            .filter(|b| b.state == RecordState::Ready);
        if !project.config.containers().any(|(k, _, _)| k == key) {
            if live.clone().next().is_some() || !service.snapshots.is_empty() {
                out.services.push(key.clone());
            }
            continue;
        }
        for branch in live {
            if let Some(owner) = branch.owner
                && out.incomplete.is_empty()
                && !instances.contains(&owner)
            {
                out.branches.push(OrphanBranch {
                    service: key.clone(),
                    branch: branch.id,
                    name: branch.name.clone(),
                    owner: Some(owner),
                    reason: "no workspace of this project has the instance that owns it".into(),
                });
            }
        }
    }
    Ok(out)
}

/// Remove everything recorded for service `key`, which the configuration no
/// longer has: its branches (containers and volumes) and snapshots by
/// recorded identity, then its catalog entry. Returns what was removed.
///
/// Refused for a service that is still configured (devflow never deletes the
/// data of a configured service) and for a key the catalog does not know.
///
/// `project` may be older than the files (a window keeps it for as long as it
/// is open), so the configuration is read again from disk, and the project's
/// identity checked, before anything is removed and once more under the
/// service's locks (resources, then the catalog lock) right before the first
/// removal: a service that was configured again meanwhile is never forgotten.
/// A configuration that cannot be read refuses as well.
pub fn forget(project: &Project, key: &str) -> Result<Vec<String>> {
    ensure!(
        !project.config.containers().any(|(k, _, _)| k == key),
        "service '{key}' is still configured in {}; remove it from the configuration first \
         (devflow never deletes the data of a configured service)",
        project.config_path.display()
    );
    ensure_unconfigured(project, key)?;
    let path = super::catalog::Store::path_for(project.id())?;
    let engine = super::catalog::read_file(&path)?
        .and_then(|catalog| catalog.services.get(key).map(|service| service.engine))
        .with_context(|| format!("the catalog records no service named '{key}'"))?;
    Service::recorded(project, key, engine)?.forget_checked(|| ensure_unconfigured(project, key))
}

/// The project as its files say now, which must be the same project.
fn fresh(project: &Project) -> Result<Project> {
    let fresh = project.reload().with_context(|| {
        format!(
            "cannot read {} again to check it before removing data; nothing was removed",
            project.config_path.display()
        )
    })?;
    ensure!(
        fresh.id() == project.id(),
        "{} is project {} now, not {}; nothing was removed",
        fresh.config_path.display(),
        fresh.id(),
        project.id()
    );
    Ok(fresh)
}

/// Is `key` still absent from the configuration on disk?
fn ensure_unconfigured(project: &Project, key: &str) -> Result<()> {
    let fresh = fresh(project)?;
    ensure!(
        !fresh.config.containers().any(|(k, _, _)| k == key),
        "service '{key}' is configured again in {}; remove it from the configuration first \
         (devflow never deletes the data of a configured service)",
        fresh.config_path.display()
    );
    Ok(())
}

/// Remove branch `branch` of the configured service `key` if it is an orphan
/// *now*: the project's files and worktrees are read again, and the branch
/// must be one that no workspace's receipt owns. A workspace that holds the
/// instance meanwhile keeps its data (`devflow service NAME drop` is the
/// explicit way to remove any branch by id).
///
/// An incomplete ownership scan (a receipt that cannot be read) proves
/// nothing: it is refused with [`OwnershipUnknown`], never read as "no owner".
pub fn drop_orphan(project: &Project, key: &str, branch: Uuid) -> Result<()> {
    let fresh = fresh(project)?;
    confirm_orphan(
        &fresh,
        &super::catalog::Store::path_for(fresh.id())?,
        key,
        branch,
    )?;
    Service::open(&fresh, key)?.remove_branch(branch)
}

/// `branch` of `key` is an orphan in the catalog at `path` with every
/// workspace's receipt read; anything else is an error.
fn confirm_orphan(fresh: &Project, path: &std::path::Path, key: &str, branch: Uuid) -> Result<()> {
    let found = orphans_in(fresh, path)?;
    if !found.is_complete() {
        return Err(OwnershipUnknown {
            problems: found.incomplete,
        }
        .into());
    }
    ensure!(
        found
            .branches
            .iter()
            .any(|o| o.service == key && o.branch == branch),
        "branch {branch} of service '{key}' is not an orphan any more: a workspace holds the \
         instance that owns it, or it was removed or the service is no longer configured; \
         nothing was dropped"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::services::catalog::{BranchRecord, Catalog, ServiceCatalog, VERSION};
    use serde_json::json;

    #[test]
    fn resolved_sources_round_trip_as_catalog_ids() {
        let id = Uuid::from_u128(0xabc);
        for source in [
            SourceRef::Empty,
            SourceRef::Branch { id },
            SourceRef::Snapshot { id },
        ] {
            assert_eq!(decode_ref(&encode_ref(source)).unwrap(), source);
        }
        assert_eq!(
            encode_ref(SourceRef::Branch { id }),
            "branch:00000000-0000-0000-0000-000000000abc"
        );
        // A name is not an identity.
        for text in [
            "",
            "workspace:main",
            "branch:main",
            "snapshot:seed@v1",
            "main",
        ] {
            assert!(decode_ref(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_data_source_names_a_branch_by_the_short_id_a_listing_shows() {
        let (_dir, root) = repo_with(&toml(
            Uuid::new_v4(),
            "[services.cache]\nengine = \"valkey\"\n",
        ));
        let project = Project::open(&root).unwrap();
        let mut records = ServiceCatalog::new(Engine::Valkey);
        let wanted = Uuid::parse_str("105a6201-aaaa-4bbb-8ccc-0123456789ab").unwrap();
        let twin = Uuid::parse_str("105a6201-bbbb-4bbb-8ccc-0123456789ab").unwrap();
        let other = Uuid::parse_str("3f0c9d11-aaaa-4bbb-8ccc-0123456789ab").unwrap();
        for id in [wanted, twin, other] {
            records.branches.push(ready_branch(id));
        }
        let mut aborted =
            ready_branch(Uuid::parse_str("77777777-aaaa-4bbb-8ccc-0123456789ab").unwrap());
        aborted.state = RecordState::Aborted;
        records.branches.push(aborted);
        let resolve = |text: &str| {
            resolve_in(
                &project,
                "cache",
                Engine::Valkey,
                &records,
                &text.parse().unwrap(),
            )
        };
        // A unique prefix, with or without dashes and in any case.
        for text in [
            "branch:3f0c9d11",
            "branch:3F0C-9D11-aaaa",
            "branch:3f0c9d11-aaaa-4bbb-8ccc-0123456789ab",
        ] {
            let (source, label) = resolve(text).unwrap();
            assert_eq!(source, SourceRef::Branch { id: other }, "{text}");
            assert_eq!(
                label,
                format!("branch:{other}"),
                "the label records the whole id"
            );
        }
        // Nine characters tell the twins apart.
        assert_eq!(
            resolve("branch:105a6201bbbb").unwrap().0,
            SourceRef::Branch { id: twin }
        );
        // Ambiguous: both candidates named by enough characters to type.
        let error = format!("{:#}", resolve("branch:105a6201").unwrap_err());
        assert!(
            error.contains("ambiguous")
                && error.contains("105a6201a")
                && error.contains("105a6201b")
                && !error.contains("3f0c9d11"),
            "{error}"
        );
        // Unknown, and an aborted branch is not one a listing shows.
        for text in ["branch:deadbeef", "branch:77777777"] {
            let error = format!("{:#}", resolve(text).unwrap_err());
            assert!(
                error.contains("no branch has an id starting with"),
                "{text}: {error}"
            );
        }
        // Too short is refused when parsed.
        assert!("branch:3f0c9d1".parse::<DataSource>().is_err());
    }

    fn ready_branch(id: Uuid) -> BranchRecord {
        serde_json::from_value(json!({
            "id": id,
            "name": "b",
            "state": "ready",
            "container_name": "c",
            "volume": "v",
            "image": "valkey/valkey:8-alpine",
            "pin": "valkey/valkey@sha256:1",
            "password": "pw", // kingfisher:ignore
            "source": SourceRef::Empty,
            "generation": 1,
            "created_at": "2026-10-11T09:00:00Z",
            "updated_at": "2026-10-11T09:00:00Z",
        }))
        .unwrap()
    }

    #[test]
    fn a_configured_service_is_never_forgotten() {
        let config = Config::parse(
            "[project]\nname = \"shop\"\nid = \"0b3c9d2e-1111-4222-8333-444455556666\"\n\
             [services.cache]\nengine = \"valkey\"\n",
        )
        .unwrap();
        let project = Project {
            root: "/tmp/shop".into(),
            git_common_dir: "/tmp/shop/.git".into(),
            config_path: "/tmp/shop/devflow.toml".into(),
            config,
        };
        let error = forget(&project, "cache").unwrap_err().to_string();
        assert!(error.contains("still configured"), "{error}");
    }

    /// A repository whose `devflow.toml` is `toml` (project id `id`).
    fn repo_with(toml: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        git2::Repository::init(&root).unwrap();
        std::fs::write(root.join("devflow.toml"), toml).unwrap();
        (dir, root)
    }

    fn toml(id: Uuid, services: &str) -> String {
        format!("[project]\nname = \"shop\"\nid = \"{id}\"\n{services}")
    }

    #[test]
    fn a_service_configured_again_on_disk_is_not_forgotten_whatever_the_loaded_project_says() {
        let id = Uuid::new_v4();
        let (_dir, root) = repo_with(&toml(id, ""));
        // What a window loaded earlier: no `cache` in the configuration.
        let loaded = Project::open(&root).unwrap();
        assert!(!loaded.config.containers().any(|(k, _, _)| k == "cache"));
        std::fs::write(
            root.join("devflow.toml"),
            toml(id, "[services.cache]\nengine = \"valkey\"\n"),
        )
        .unwrap();
        let error = forget(&loaded, "cache").unwrap_err().to_string();
        assert!(error.contains("configured again"), "{error}");

        // The same path is another project now.
        std::fs::write(root.join("devflow.toml"), toml(Uuid::new_v4(), "")).unwrap();
        let error = forget(&loaded, "cache").unwrap_err().to_string();
        assert!(error.contains("nothing was removed"), "{error}");

        // A configuration that cannot be read proves nothing: refused.
        std::fs::remove_file(root.join("devflow.toml")).unwrap();
        let error = forget(&loaded, "cache").unwrap_err().to_string();
        assert!(error.contains("cannot read"), "{error}");
    }

    #[test]
    fn only_a_branch_that_is_an_orphan_now_is_dropped_as_one() {
        let id = Uuid::new_v4();
        let (_dir, root) = repo_with(&toml(id, "[services.cache]\nengine = \"valkey\"\n"));
        let loaded = Project::open(&root).unwrap();
        // No catalog lists the branch as an orphan: refused before Docker is asked.
        let error = drop_orphan(&loaded, "cache", Uuid::new_v4())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an orphan any more"), "{error}");
        // The same path is another project now.
        std::fs::write(root.join("devflow.toml"), toml(Uuid::new_v4(), "")).unwrap();
        let error = drop_orphan(&loaded, "cache", Uuid::new_v4())
            .unwrap_err()
            .to_string();
        assert!(error.contains("nothing was removed"), "{error}");
    }

    /// A project with a linked `feature` worktree, a Valkey `cache` branch
    /// owned by `feature`'s instance, and the catalog file that records it.
    fn owned_branch() -> (tempfile::TempDir, Project, std::path::PathBuf, Uuid, Uuid) {
        let (dir, root) = repo_with(&toml(
            Uuid::new_v4(),
            "[services.cache]\nengine = \"valkey\"\n",
        ));
        let repo = git2::Repository::open(&root).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        repo.worktree("feature", &dir.path().join("feature"), None)
            .unwrap();
        let project = Project::open(&root).unwrap();
        let feature = worktree::find(&project, "feature").unwrap().unwrap();
        let receipt = Receipt::new(project.id(), "feature", None);
        let instance = receipt.instance_id;
        receipt.save(&feature.git_dir).unwrap();
        let branch_id = Uuid::new_v4();
        let branch: BranchRecord = serde_json::from_value(json!({
            "id": branch_id,
            "name": "feature",
            "owner": instance,
            "state": "ready",
            "container_name": "c-feature",
            "volume": "v-feature",
            "image": "valkey/valkey:8-alpine",
            "pin": "valkey/valkey@sha256:1",
            "password": "pw", // kingfisher:ignore
            "source": {"kind": "empty"},
            "generation": 1,
            "created_at": "2026-10-11T09:00:00Z",
            "updated_at": "2026-10-11T09:00:00Z",
        }))
        .unwrap();
        let mut records = ServiceCatalog::new(Engine::Valkey);
        records.branches.push(branch);
        let catalog = Catalog {
            version: VERSION,
            project_id: project.id(),
            docker: "daemon".into(),
            services: [("cache".to_string(), records)].into(),
        };
        let path = dir.path().join("catalog.json");
        std::fs::write(&path, serde_json::to_vec(&catalog).unwrap()).unwrap();
        (dir, project, path, branch_id, instance)
    }

    #[test]
    fn a_receipt_that_cannot_be_read_makes_ownership_unknown_not_absent() {
        let (_dir, project, path, branch, _) = owned_branch();
        let feature = worktree::find(&project, "feature").unwrap().unwrap();

        // Read: the workspace holds the instance, so the branch is no orphan.
        let found = orphans_in(&project, &path).unwrap();
        assert!(
            found.is_complete() && found.branches.is_empty(),
            "{found:?}"
        );
        let error = confirm_orphan(&project, &path, "cache", branch)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an orphan any more"), "{error}");

        // Unreadable, malformed or newer: the owner may be in it. The scan is
        // incomplete, nothing is called an orphan, and a drop is refused with
        // the typed error.
        let receipt = Receipt::path(&feature.git_dir);
        for content in [
            "{ not json".to_string(),
            std::fs::read_to_string(&receipt).unwrap().replacen(
                "\"version\": 2",
                "\"version\": 99",
                1,
            ),
        ] {
            std::fs::write(&receipt, content).unwrap();
            let found = orphans_in(&project, &path).unwrap();
            assert!(!found.is_complete(), "{found:?}");
            assert!(found.branches.is_empty(), "{found:?}");
            assert_eq!(found.incomplete.len(), 1);
            assert_eq!(found.incomplete[0].workspace, "feature");
            assert!(!found.incomplete[0].reason.is_empty());
            let error = confirm_orphan(&project, &path, "cache", branch).unwrap_err();
            let unknown = error
                .downcast_ref::<OwnershipUnknown>()
                .unwrap_or_else(|| panic!("not typed: {error:#}"));
            assert_eq!(unknown.problems, found.incomplete);
        }

        // With a receipt that is a directory, not a file: still unknown.
        std::fs::remove_file(&receipt).unwrap();
        std::fs::create_dir(&receipt).unwrap();
        assert!(!orphans_in(&project, &path).unwrap().is_complete());

        // Gone: a workspace without a receipt owns no instance, so the branch
        // is a real orphan and is confirmed as one.
        std::fs::remove_dir(&receipt).unwrap();
        let found = orphans_in(&project, &path).unwrap();
        assert!(found.is_complete());
        assert_eq!(found.branches.len(), 1);
        confirm_orphan(&project, &path, "cache", branch).unwrap();
    }
}
