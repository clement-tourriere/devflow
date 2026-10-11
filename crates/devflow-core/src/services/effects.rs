//! What an operation does to the data it copies from, before it is asked for.
//!
//! A frontend that confirms a snapshot or a reset states this in its
//! confirmation instead of guessing from an engine's name. Valkey and Redis
//! save a running source without pausing it, so a copy changes nothing for
//! whoever uses the source. MySQL cannot: its source is shut down cleanly for
//! the copy and started again (see `exec`), so every connection to it is
//! interrupted for a few seconds. [`clone_stops_source`] is that fact as
//! engine metadata; [`effects`] applies it to one operation of one project.
//!
//! Nothing here asks Docker. Whether the source server runs right now is the
//! caller's to say (its listing knows), and the sentence it builds should
//! read "if it is running" without that.
use super::catalog::{Catalog, RecordState, ServiceCatalog, SourceRef, Step, Store, read_file};
use super::workspace::binding;
use crate::config::Engine;
use crate::db::DataSource;
use crate::project::Project;
use crate::receipt::Receipt;
use crate::setup;
use crate::worktree::{self, Workspace};
use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

/// An operation on a workspace's branch of a container service, as it would
/// be requested (`setup::service_snapshot`, `setup::service_reset`).
#[derive(Debug, Clone, Copy)]
pub enum Operation<'a> {
    /// Seal a snapshot of the branch bound to `workspace`.
    Snapshot { workspace: &'a Workspace },
    /// Replace the branch bound to `workspace` with a fresh clone of `to`, or
    /// of the branch's recorded source when `to` is `None`.
    Reset {
        workspace: &'a Workspace,
        to: Option<&'a DataSource>,
    },
    /// A reset whose source was already resolved to a catalog identity
    /// (`setup::reset_source`): the effects of exactly what the reset will be
    /// held to (`BranchExpect::source`), not of a name looked up again.
    ResetFrom { source: SourceRef },
}

/// What an operation does besides what its name says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Effects {
    /// The branch whose server the copy stops, if it is running, and starts
    /// again afterwards (MySQL): its connections are interrupted for a few
    /// seconds. `None` when the source keeps serving, or there is no source
    /// (an empty or snapshot source, or an engine that copies a saved dump).
    pub stops_source: Option<StoppedSource>,
    /// How long the last such stop on this service lasted, in milliseconds,
    /// when one was measured.
    pub last_downtime_ms: Option<u64>,
}

/// The branch an operation copies from and stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoppedSource {
    pub branch: Uuid,
    /// The branch's display name.
    pub name: String,
    /// The workspace whose receipt carries the instance that owns the branch.
    pub workspace: Option<String>,
}

/// Does cloning a branch of `engine` stop the source's server? (MySQL: the
/// data directory is copied whole, so the server is shut down cleanly first.)
pub fn clone_stops_source(engine: Engine) -> bool {
    engine == Engine::Mysql
}

/// What `op` on service `key` of `project` would do to its source, from the
/// project's catalog and receipts (no Docker).
pub fn effects(project: &Project, key: &str, op: Operation<'_>) -> Result<Effects> {
    // An engine that copies a saved dump has nothing to look up.
    if !clone_stops_source(engine_of(project, key)?) {
        return Ok(Effects::default());
    }
    let catalog = read_file(&Store::path_for(project.id())?)?;
    effects_in(project, key, op, catalog.as_ref())
}

fn engine_of(project: &Project, key: &str) -> Result<Engine> {
    project
        .config
        .containers()
        .find(|(k, _, _)| *k == key)
        .map(|(_, engine, _)| engine)
        .with_context(|| {
            format!(
                "{} has no container service '{key}'",
                project.config_path.display()
            )
        })
}

fn effects_in(
    project: &Project,
    key: &str,
    op: Operation<'_>,
    catalog: Option<&Catalog>,
) -> Result<Effects> {
    let engine = engine_of(project, key)?;
    let none = Effects::default();
    if !clone_stops_source(engine) {
        return Ok(none);
    }
    let Some(records) = catalog.and_then(|c| c.services.get(key)) else {
        return Ok(none);
    };
    let Some(source) = source_branch(project, key, engine, op, records)? else {
        return Ok(none);
    };
    let Some(branch) = records
        .branch(source)
        .filter(|b| b.state == RecordState::Ready)
    else {
        return Ok(none);
    };
    Ok(Effects {
        stops_source: Some(StoppedSource {
            branch: branch.id,
            name: branch.name.clone(),
            workspace: branch.owner.and_then(|owner| workspace_of(project, owner)),
        }),
        last_downtime_ms: last_downtime(records),
    })
}

/// The branch `op` copies from, when it copies from one.
fn source_branch(
    project: &Project,
    key: &str,
    engine: Engine,
    op: Operation<'_>,
    records: &ServiceCatalog,
) -> Result<Option<Uuid>> {
    let bound = |workspace: &Workspace| -> Result<Option<Uuid>> {
        let receipt = setup::receipt(project, workspace)?;
        Ok(receipt
            .as_ref()
            .and_then(|r| binding(r, key, engine))
            .and_then(|b| b.branch_id))
    };
    Ok(match op {
        Operation::Snapshot { workspace } => bound(workspace)?,
        Operation::Reset { workspace, to } => match to {
            // The branch's own immutable provenance.
            None => bound(workspace)?
                .and_then(|id| records.branch(id))
                .and_then(|own| match own.source {
                    SourceRef::Branch { id } => Some(id),
                    SourceRef::Empty | SourceRef::Snapshot { .. } => None,
                }),
            Some(DataSource::Workspace { name }) => match worktree::find(project, name)? {
                Some(other) => bound(&other)?,
                None => None,
            },
            Some(branch @ (DataSource::Branch { .. } | DataSource::BranchPrefix { .. })) => {
                // An id that does not name a branch is reported by the
                // operation itself; there is nothing to warn about here.
                branch
                    .branch_id(records.branches.iter().map(|b| b.id))
                    .ok()
                    .flatten()
            }
            // Nothing runs on a snapshot, and an empty root has no source.
            Some(DataSource::Empty | DataSource::Snapshot { .. }) => None,
        },
        Operation::ResetFrom { source } => match source {
            SourceRef::Branch { id } => Some(id),
            SourceRef::Empty | SourceRef::Snapshot { .. } => None,
        },
    })
}

/// The workspace whose receipt carries `instance`.
fn workspace_of(project: &Project, instance: Uuid) -> Option<String> {
    worktree::list(project)
        .ok()?
        .into_iter()
        .find(|w| {
            Receipt::load(&w.git_dir)
                .ok()
                .flatten()
                .is_some_and(|r| r.instance_id == instance)
        })
        .map(|w| w.name)
}

/// The downtime of the most recent finished operation that measured one.
fn last_downtime(records: &ServiceCatalog) -> Option<u64> {
    records
        .journal
        .iter()
        .filter(|op| op.step == Step::Done && op.downtime_ms.is_some())
        .max_by_key(|op| op.updated_at)
        .and_then(|op| op.downtime_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{CloneReport, ServiceBinding};
    use crate::services::catalog::{BranchRecord, Op, OpKind, VERSION};
    use serde_json::json;
    use std::collections::BTreeMap;

    struct Rig {
        _dir: tempfile::TempDir,
        project: Project,
        main: Workspace,
        feature: Workspace,
    }

    fn branch(id: Uuid, name: &str, owner: Option<Uuid>, source: SourceRef) -> BranchRecord {
        serde_json::from_value(json!({
            "id": id,
            "name": name,
            "owner": owner,
            "state": "ready",
            "container_name": format!("c-{name}"),
            "volume": format!("v-{name}"),
            "image": "mysql:8.4",
            "pin": "mysql@sha256:1",
            "password": "pw", // kingfisher:ignore
            "source": source,
            "generation": 1,
            "created_at": "2026-10-11T09:00:00Z",
            "updated_at": "2026-10-11T09:00:00Z",
        }))
        .unwrap()
    }

    fn bind(workspace: &Workspace, project: &Project, key: &str, engine: Engine, branch: Uuid) {
        let mut receipt = Receipt::new(project.id(), &workspace.name, None);
        receipt.services.insert(
            key.into(),
            ServiceBinding::container(
                engine,
                branch,
                "mysql://app:pw@127.0.0.1:3307/app".into(), // kingfisher:ignore
                None,
                "empty".into(),
                CloneReport {
                    method: None,
                    captured_at: None,
                    allocated_bytes: None,
                    source_downtime_ms: None,
                },
                None,
            ),
        );
        receipt.save(&workspace.git_dir).unwrap();
    }

    /// A repository with a `main` and a linked `feature` worktree, and the
    /// services `shop-data` (MySQL) and `cache` (Valkey) configured.
    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
        repo.worktree("feature", &dir.path().join("feature"), None)
            .unwrap();
        std::fs::write(
            root.join("devflow.toml"),
            format!(
                "[project]\nname = \"shop\"\nid = \"{}\"\n\
                 [services.shop-data]\nengine = \"mysql\"\n\
                 [services.cache]\nengine = \"valkey\"\n",
                Uuid::new_v4()
            ),
        )
        .unwrap();
        let project = Project::open(&root).unwrap();
        let all = worktree::list(&project).unwrap();
        let main = all.iter().find(|w| w.is_default).cloned().unwrap();
        let feature = all.iter().find(|w| w.name == "feature").cloned().unwrap();
        Rig {
            _dir: dir,
            project,
            main,
            feature,
        }
    }

    fn catalog_of(rig: &Rig, key: &str, engine: Engine, records: ServiceCatalog) -> Catalog {
        Catalog {
            version: VERSION,
            project_id: rig.project.id(),
            docker: "daemon".into(),
            services: BTreeMap::from([(key.to_string(), {
                let mut service = records;
                service.engine = engine;
                service
            })]),
        }
    }

    fn done(downtime: Option<u64>, minutes: i64) -> Op {
        let mut op = Op::new(OpKind::Snapshot, Uuid::new_v4());
        op.step = Step::Done;
        op.downtime_ms = downtime;
        op.updated_at = chrono::Utc::now() + chrono::Duration::minutes(minutes);
        op
    }

    #[test]
    fn a_mysql_snapshot_or_reset_from_a_running_workspace_says_whose_server_it_stops() {
        let rig = rig();
        let (main_instance, feature_instance) = (Uuid::new_v4(), Uuid::new_v4());
        let (main_branch, feature_branch) = (Uuid::new_v4(), Uuid::new_v4());
        let mut records = ServiceCatalog::new(Engine::Mysql);
        records.branches.push(branch(
            main_branch,
            "main",
            Some(main_instance),
            SourceRef::Empty,
        ));
        // The feature branch was cloned from main's.
        records.branches.push(branch(
            feature_branch,
            "feature",
            Some(feature_instance),
            SourceRef::Branch { id: main_branch },
        ));
        records.journal = vec![done(Some(1500), 0), done(Some(2300), 5), done(None, 9)];
        let catalog = catalog_of(&rig, "shop-data", Engine::Mysql, records);
        for (workspace, instance, branch) in [
            (&rig.main, main_instance, main_branch),
            (&rig.feature, feature_instance, feature_branch),
        ] {
            bind(workspace, &rig.project, "shop-data", Engine::Mysql, branch);
            let mut receipt = Receipt::load(&workspace.git_dir).unwrap().unwrap();
            receipt.instance_id = instance;
            receipt.save(&workspace.git_dir).unwrap();
        }
        fn effects_of(rig: &Rig, catalog: &Catalog, op: Operation<'_>) -> Effects {
            effects_in(&rig.project, "shop-data", op, Some(catalog)).unwrap()
        }
        let of = |op: Operation<'_>| effects_of(&rig, &catalog, op);

        // A snapshot of main stops main's own server.
        let found = of(Operation::Snapshot {
            workspace: &rig.main,
        });
        let stopped = found.stops_source.expect("the snapshot stops its source");
        assert_eq!(
            (
                stopped.branch,
                stopped.name.as_str(),
                stopped.workspace.as_deref()
            ),
            (main_branch, "main", Some(rig.main.name.as_str()))
        );
        assert_eq!(
            found.last_downtime_ms,
            Some(2300),
            "the newest measured stop"
        );

        // A reset of feature from main's data stops main's server, not feature's.
        let from_main = of(Operation::Reset {
            workspace: &rig.feature,
            to: Some(&DataSource::Workspace {
                name: rig.main.name.clone(),
            }),
        });
        assert_eq!(from_main.stops_source.unwrap().branch, main_branch);
        // Its recorded source is main's branch too.
        let recorded = of(Operation::Reset {
            workspace: &rig.feature,
            to: None,
        });
        assert_eq!(
            recorded.stops_source.unwrap().workspace.as_deref(),
            Some(rig.main.name.as_str())
        );
        // Main's recorded source is empty: nothing to stop.
        let own = of(Operation::Reset {
            workspace: &rig.main,
            to: None,
        });
        assert_eq!(own, Effects::default());
        // Empty data and snapshots have no server.
        for to in [
            DataSource::Empty,
            DataSource::Snapshot {
                name: "seed".into(),
                version: None,
            },
        ] {
            let found = of(Operation::Reset {
                workspace: &rig.feature,
                to: Some(&to),
            });
            assert_eq!(found, Effects::default(), "{to:?}");
        }
        // A branch by id is named by its identity.
        let by_id = of(Operation::Reset {
            workspace: &rig.feature,
            to: Some(&DataSource::Branch { id: feature_branch }),
        });
        assert_eq!(
            by_id.stops_source.clone().unwrap().workspace.as_deref(),
            Some(rig.feature.name.as_str())
        );
        // So is the short id a listing shows.
        let by_short_id = of(Operation::Reset {
            workspace: &rig.feature,
            to: Some(&DataSource::BranchPrefix {
                prefix: crate::ids::short(feature_branch),
            }),
        });
        assert_eq!(by_short_id, by_id);
        // An already resolved source: the identity decides, whatever its
        // workspace's name points at now.
        let resolved = of(Operation::ResetFrom {
            source: SourceRef::Branch { id: main_branch },
        });
        assert_eq!(resolved.stops_source.unwrap().branch, main_branch);
        for source in [SourceRef::Empty, SourceRef::Snapshot { id: Uuid::new_v4() }] {
            assert_eq!(
                of(Operation::ResetFrom { source }),
                Effects::default(),
                "{source:?}"
            );
        }
    }

    #[test]
    fn engines_that_copy_a_saved_dump_stop_nothing() {
        let rig = rig();
        let branch_id = Uuid::new_v4();
        let mut records = ServiceCatalog::new(Engine::Valkey);
        records.branches.push(branch(
            branch_id,
            "main",
            Some(Uuid::new_v4()),
            SourceRef::Empty,
        ));
        let catalog = catalog_of(&rig, "cache", Engine::Valkey, records);
        bind(&rig.main, &rig.project, "cache", Engine::Valkey, branch_id);
        let found = effects_in(
            &rig.project,
            "cache",
            Operation::Snapshot {
                workspace: &rig.main,
            },
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(found, Effects::default());
        assert!(clone_stops_source(Engine::Mysql));
        assert!(!clone_stops_source(Engine::Valkey) && !clone_stops_source(Engine::Redis));
        // A service the configuration does not have is an error, not "no effect".
        assert!(
            effects_in(
                &rig.project,
                "nope",
                Operation::Snapshot {
                    workspace: &rig.main
                },
                None
            )
            .is_err()
        );
    }
}
