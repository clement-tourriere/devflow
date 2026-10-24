//! The data a removal has to account for.
//!
//! A workspace's receipt is the only place that says which branches it owns,
//! and it goes with the worktree. A removal therefore reads every branch the
//! receipt names before it changes anything, and deletes the receipt (with
//! the worktree) only once each of them is accounted for: dropped, found to
//! belong to somebody else, or found not to exist. The ones it names are
//!
//! - the bindings the workspace owns (`receipt.services`), including those of
//!   services the configuration no longer has;
//! - the branch being created (`receipt.creating`), recorded before the effect
//!   so that a crash between the creation and the binding can be recovered by
//!   its identity: a removal that skipped it would leak the branch. It stays
//!   recoverable when its service has left the configuration since: the
//!   catalog of the container services knows a key's engine, and the one
//!   PostgreSQL server of the project is asked for the id;
//! - a database branch an earlier build bound (`receipt.unmapped`).
//!
//! Planning ([`plan`]) only looks; it drops nothing. Execution ([`execute`])
//! uses the backends' own journal-aware operations, each of which decides on
//! the branch again under the lock it already holds for the deletion (the
//! project's advisory lock for PostgreSQL, the branch's resource lock for a
//! container service), so a branch that changed hands since the plan is left
//! alone and no helper is called while a lock is held that would take it
//! again.
//!
//! What cannot be looked at is not "gone". A bound branch whose backend cannot
//! be asked stops the removal before any effect, forceably (`force` leaves it
//! and says so). A branch recorded as about to be created that cannot be
//! resolved stops it for good: the receipt dying with the worktree would leave
//! a branch nothing names, and `force` does not accept that; resuming setup
//! (which recovers it by its recorded identity) or reconciling the service is
//! the way on.
use super::{connect, recoverable};
use crate::config::Engine;
use crate::db::Server;
use crate::project::Project;
use crate::receipt::Receipt;
use crate::remedy::Remedy;
use crate::services::catalog::Store;
use crate::services::{self, Service};
use crate::worktree::{Workspace, protected_with};
use anyhow::{Result, ensure};
use uuid::Uuid;

/// A branch of the workspace, and where the receipt has it.
// (`primary` below is the configured postgres service, whose branch goes first.)
#[derive(Debug, Clone, PartialEq, Eq)]
struct Named {
    /// How to name it in a message.
    what: String,
    /// `None` for a branch recorded before its creation by a devflow that did
    /// not record the engine with it.
    engine: Option<Engine>,
    id: Uuid,
    /// Only recorded before its creation: it may never have been made.
    pending: bool,
    /// The service key the catalog knows it under (container services).
    key: Option<String>,
}

/// Every branch `receipt` names that this workspace may own, the configured
/// postgres service's first.
fn named(project: &Project, receipt: &Receipt) -> Vec<Named> {
    let primary = project.config.postgres().map(|(key, _)| key);
    let mut all: Vec<Named> = receipt
        .services
        .iter()
        .filter(|(_, binding)| binding.owned)
        .filter_map(|(key, binding)| {
            Some(Named {
                what: format!("service '{key}'"),
                engine: Some(binding.engine),
                id: binding.branch_id?,
                pending: false,
                key: Some(key.clone()),
            })
        })
        .collect();
    all.sort_by_key(|n| n.key.as_deref() != primary);
    // The backend the creation was begun on is the one recorded with it. The
    // service's configuration is not where to look: it may have changed since
    // (`db` was a PostgreSQL service and is a Valkey one now). A record from
    // before the engine was written has none, and is looked for everywhere.
    if let Some(pending) = &receipt.creating
        && let Some(id) = recoverable(receipt, &pending.service)
    {
        all.push(Named {
            what: format!("service '{}'", pending.service),
            engine: pending.engine,
            id,
            pending: true,
            key: Some(pending.service.clone()),
        });
    }
    if let Some(data) = &receipt.unmapped
        && data.owned
        && let Some(id) = data.branch_id
    {
        all.push(Named {
            what: "a database from an earlier build".to_string(),
            engine: Some(Engine::Postgres),
            id,
            pending: false,
            key: None,
        });
    }
    all
}

/// What the removal drops, decided before it changes anything.
#[derive(Default)]
pub(super) struct DataPlan {
    instance: Option<Uuid>,
    postgres: Option<Server>,
    postgres_ids: Vec<Uuid>,
    containers: Vec<(Service, Uuid)>,
}

impl DataPlan {
    /// What the plan will drop, for a message that says what remains.
    pub(super) fn describe(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .postgres_ids
            .iter()
            .map(|id| format!("database branch {id}"))
            .collect();
        all.extend(
            self.containers
                .iter()
                .map(|(svc, id)| format!("branch {id} of service '{}'", svc.key)),
        );
        all
    }
}

/// A branch that stops the removal, and what the person can do about it.
struct Blocking {
    reason: String,
    remedies: Vec<Remedy>,
}

/// What the problems found by [`plan`] come to.
#[derive(Default)]
struct Problems {
    /// `force` leaves these (a bound branch whose backend cannot be asked).
    forceable: Vec<String>,
    /// `force` does not (a branch recorded before its creation that cannot be
    /// resolved).
    blocking: Vec<Blocking>,
}

impl Problems {
    fn settle(self, workspace: &Workspace, force: bool, warnings: &mut Vec<String>) -> Result<()> {
        if !self.blocking.is_empty() {
            let reason = self
                .blocking
                .iter()
                .map(|b| b.reason.as_str())
                .chain(self.forceable.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            let mut remedies: Vec<Remedy> = Vec::new();
            for remedy in self.blocking.into_iter().flat_map(|b| b.remedies) {
                if !remedies.contains(&remedy) {
                    remedies.push(remedy);
                }
            }
            return Err(protected_with(&workspace.name, reason, false, remedies));
        }
        if self.forceable.is_empty() {
            return Ok(());
        }
        if force {
            warnings.extend(
                self.forceable
                    .into_iter()
                    .map(|reason| format!("{reason}; it was left alone")),
            );
            return Ok(());
        }
        Err(protected_with(
            &workspace.name,
            format!(
                "{}; remove the workspace and keep its data, or force the removal to leave it",
                self.forceable.join("; ")
            ),
            true,
            Vec::new(),
        ))
    }
}

/// What one backend says about a branch.
enum Lookup<T> {
    Found(T),
    /// The backend could be asked, and does not have it.
    Absent,
    /// It could not be asked, or not answered.
    Unknown(String),
}

/// What the inspections share.
struct Cx<'a> {
    project: &'a Project,
    workspace: &'a Workspace,
    instance: Uuid,
}

/// The project's PostgreSQL server, asked for once.
#[derive(Default)]
struct Pg {
    state: Option<Result<Option<Server>, String>>,
}

impl Pg {
    fn get(&mut self, project: &Project) -> &Result<Option<Server>, String> {
        self.state
            .get_or_insert_with(|| connect(project).map_err(|e| format!("{e:#}")))
    }
}

impl Cx<'_> {
    /// A branch recorded before its creation that cannot be resolved: setup
    /// would recover it by its identity, reconciling finishes what the backend
    /// left half done.
    fn unresolved(&self, named: &Named, why: &str, key: Option<&str>, postgres: bool) -> Blocking {
        let root = self.project.root.clone();
        let mut remedies = vec![Remedy::Setup {
            workspace: self.workspace.path.clone(),
        }];
        if postgres {
            remedies.push(Remedy::ReconcileDatabases { root: root.clone() });
        }
        if let Some(key) = key {
            remedies.push(Remedy::ReconcileService {
                root,
                key: key.to_string(),
            });
        }
        Blocking {
            reason: format!(
                "branch {} was recorded as about to be created for {} when setup stopped, and \
                 it cannot be looked for ({why}); removing the workspace now would leave a \
                 branch nothing names. Resume the setup so it is bound, or reconcile the service",
                named.id, named.what
            ),
            remedies,
        }
    }

    fn inspect_postgres(
        &self,
        pg: &mut Pg,
        branch: &Named,
        plan: &mut DataPlan,
        problems: &mut Problems,
        warnings: &mut Vec<String>,
    ) {
        let cannot = |why: String, problems: &mut Problems| {
            if branch.pending {
                problems
                    .blocking
                    .push(self.unresolved(branch, &why, None, true));
            } else {
                problems.forceable.push(why);
            }
        };
        match pg.get(self.project) {
            Ok(Some(server)) => match server.branch(branch.id) {
                Ok(Some(info)) if info.instance_id == Some(self.instance) => {
                    plan.postgres_ids.push(branch.id);
                }
                Ok(Some(_)) => warnings.push(format!(
                    "database branch {} was created by another instance and was left alone",
                    branch.id
                )),
                // Recorded before the creation, which never happened.
                Ok(None) if branch.pending => {}
                Ok(None) => {
                    warnings.push(format!("database branch {} no longer exists", branch.id))
                }
                Err(e) => cannot(
                    format!(
                        "cannot inspect the database branch of {}: {e:#}",
                        branch.what
                    ),
                    problems,
                ),
            },
            Ok(None) => cannot(
                format!(
                    "the project has no postgres service any more, so the database branch {} of \
                     {} cannot be dropped",
                    branch.id, branch.what
                ),
                problems,
            ),
            Err(why) => cannot(
                format!(
                    "cannot reach the database server to drop the branch of {}: {why}",
                    branch.what
                ),
                problems,
            ),
        }
    }

    fn inspect_container(
        &self,
        branch: &Named,
        key: &str,
        engine: Engine,
        plan: &mut DataPlan,
        problems: &mut Problems,
        warnings: &mut Vec<String>,
    ) {
        let inspected = Service::recorded(self.project, key, engine)
            .and_then(|svc| Ok((svc.branch(branch.id)?, svc)));
        match inspected {
            Ok((Some(record), svc)) if record.owner == Some(self.instance) => {
                plan.containers.push((svc, branch.id));
            }
            Ok((Some(_), _)) => warnings.push(format!(
                "branch {} of service '{key}' was created by another instance and was left alone",
                branch.id
            )),
            Ok((None, _)) if branch.pending => {}
            Ok((None, _)) => warnings.push(format!(
                "branch {} of service '{key}' no longer exists",
                branch.id
            )),
            Err(e) if branch.pending => {
                problems
                    .blocking
                    .push(self.unresolved(branch, &format!("{e:#}"), Some(key), false))
            }
            Err(e) => problems.forceable.push(format!(
                "cannot inspect the branch of service '{key}': {e:#}"
            )),
        }
    }
}

/// Look at every branch the receipt names (see the module documentation).
/// Nothing is dropped here.
pub(super) fn plan(
    project: &Project,
    workspace: &Workspace,
    receipt: &Receipt,
    force: bool,
    warnings: &mut Vec<String>,
) -> Result<DataPlan> {
    let cx = Cx {
        project,
        workspace,
        instance: receipt.instance_id,
    };
    let mut plan = DataPlan {
        instance: Some(cx.instance),
        ..DataPlan::default()
    };
    let mut problems = Problems::default();
    let mut pg = Pg::default();

    let (unplaced, placed): (Vec<Named>, Vec<Named>) = named(project, receipt)
        .into_iter()
        .partition(|n| n.engine.is_none());
    let (postgres, containers): (Vec<Named>, Vec<Named>) = placed
        .into_iter()
        .partition(|n| n.engine == Some(Engine::Postgres));

    for branch in &postgres {
        cx.inspect_postgres(&mut pg, branch, &mut plan, &mut problems, warnings);
    }
    for branch in &containers {
        let (Some(key), Some(engine)) = (branch.key.as_deref(), branch.engine) else {
            continue;
        };
        cx.inspect_container(branch, key, engine, &mut plan, &mut problems, warnings);
    }

    // Recorded before the creation by a devflow that did not record the backend
    // with it. The configuration says where the service is now, which is not
    // where the branch was begun (a PostgreSQL service can have become a
    // Valkey one), so absence from the configured backend proves nothing: both
    // are looked in, the project's one PostgreSQL server by id and the
    // container catalog by the key. Only a branch both could be asked about and
    // neither has was never made; one that either has is decided there; the
    // rest cannot be resolved.
    for branch in &unplaced {
        let Some(key) = branch.key.as_deref() else {
            continue;
        };
        let in_postgres = match pg.get(project) {
            Ok(Some(server)) => match server.branch(branch.id) {
                Ok(Some(info)) => Lookup::Found(info),
                Ok(None) => Lookup::Absent,
                Err(e) => Lookup::Unknown(format!("{e:#}")),
            },
            Ok(None) => Lookup::Unknown(
                "the project has no postgres service any more, so no server can be asked"
                    .to_string(),
            ),
            Err(why) => Lookup::Unknown(why.clone()),
        };
        let in_catalog = match services::docker::daemon_id()
            .and_then(|daemon| Store::open(project.id(), &daemon)?.load())
        {
            Ok(catalog) => match catalog.services.get(key) {
                Some(entry) => match Service::recorded(project, key, entry.engine)
                    .and_then(|svc| Ok((svc.branch(branch.id)?, svc)))
                {
                    Ok((Some(record), svc)) => Lookup::Found((record, svc)),
                    Ok((None, _)) => Lookup::Absent,
                    Err(e) => Lookup::Unknown(format!("{e:#}")),
                },
                None => Lookup::Absent,
            },
            Err(e) => Lookup::Unknown(format!("{e:#}")),
        };
        match (in_postgres, in_catalog) {
            (Lookup::Found(info), _) => {
                if info.instance_id == Some(cx.instance) {
                    plan.postgres_ids.push(branch.id);
                } else {
                    warnings.push(format!(
                        "database branch {} was created by another instance and was left alone",
                        branch.id
                    ));
                }
            }
            (_, Lookup::Found((record, svc))) => {
                if record.owner == Some(cx.instance) {
                    plan.containers.push((svc, branch.id));
                } else {
                    warnings.push(format!(
                        "branch {} of service '{key}' was created by another instance and was \
                         left alone",
                        branch.id
                    ));
                }
            }
            (Lookup::Absent, Lookup::Absent) => {}
            (postgres, catalog) => {
                let why = match (postgres, catalog) {
                    (Lookup::Unknown(why), _) | (_, Lookup::Unknown(why)) => why,
                    _ => "it was not found".to_string(),
                };
                problems
                    .blocking
                    .push(cx.unresolved(branch, &why, Some(key), true));
            }
        }
    }

    problems.settle(workspace, force, warnings)?;
    if !plan.postgres_ids.is_empty() {
        plan.postgres = match pg.state.take() {
            Some(Ok(server)) => server,
            _ => None,
        };
    }
    Ok(plan)
}

/// A drop that failed part-way, with what was already dropped and what is left.
pub(super) struct Failed {
    pub cause: anyhow::Error,
    pub done: Vec<String>,
    pub remains: Vec<String>,
    /// `force` goes on past the cause.
    pub forceable: bool,
}

/// Drop what [`plan`] found, by recorded identity, with the backends'
/// journal-aware operations. Each is decided again under the lock it holds for
/// the deletion, so a branch that changed hands since the plan is left alone.
/// With `force` a failure is a warning; without it a [`Failed`] that stops the
/// removal before the worktree goes. The first branch dropped is returned, with
/// what was dropped, for a message.
pub(super) fn execute(
    plan: DataPlan,
    force: bool,
    warnings: &mut Vec<String>,
) -> Result<(Option<Uuid>, Vec<String>), Failed> {
    let Some(instance) = plan.instance else {
        return Ok((None, Vec::new()));
    };
    let mut dropped = None;
    let mut done: Vec<String> = Vec::new();
    let mut remaining: Vec<String> = plan.describe();
    let failed = |cause: anyhow::Error, done: &[String], remaining: &[String]| Failed {
        cause,
        done: done.to_vec(),
        remains: remaining.to_vec(),
        forceable: true,
    };
    if let Some(server) = &plan.postgres {
        for id in plan.postgres_ids {
            let label = format!("database branch {id}");
            match server.drop_branch_owned_by(id, instance) {
                Ok(true) => {
                    dropped.get_or_insert(id);
                    done.push(format!("dropped {label}"));
                    remaining.retain(|r| *r != label);
                }
                Ok(false) => remaining.retain(|r| *r != label),
                Err(e) if force => {
                    warnings.push(format!("could not drop database branch {id}: {e:#}"));
                    remaining.retain(|r| *r != label);
                }
                Err(e) => {
                    return Err(failed(
                        e.context(format!("cannot drop database branch {id}")),
                        &done,
                        &remaining,
                    ));
                }
            }
        }
    }
    for (svc, id) in plan.containers {
        let label = format!("branch {id} of service '{}'", svc.key);
        let removed = svc.remove_branch_if(id, |record| {
            ensure!(
                record.owner == Some(instance),
                "branch {id} was not created by this workspace's instance"
            );
            Ok(())
        });
        match removed {
            Ok(()) => {
                dropped.get_or_insert(id);
                done.push(format!("dropped {label}"));
                remaining.retain(|r| *r != label);
            }
            // Recovered away meanwhile (an interrupted creation is rolled back
            // before the branch is looked at): nothing is left to drop.
            Err(_) if matches!(svc.branch(id), Ok(None)) => remaining.retain(|r| *r != label),
            Err(e) if force => {
                warnings.push(format!(
                    "could not drop branch {id} of service '{}': {e:#}",
                    svc.key
                ));
                remaining.retain(|r| *r != label);
            }
            Err(e) => {
                return Err(failed(
                    e.context(format!("cannot drop branch {id} of service '{}'", svc.key)),
                    &done,
                    &remaining,
                ));
            }
        }
    }
    Ok((dropped, done))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::{DataBinding, PendingBranch, ServiceBinding};

    fn project(extra: &str) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git2::Repository::init(&root).unwrap();
        std::fs::write(
            root.join("devflow.toml"),
            format!(
                "[project]\nname = \"shop\"\nid = \"{}\"\n{extra}\n",
                Uuid::new_v4()
            ),
        )
        .unwrap();
        let project = Project::open(&root).unwrap();
        (dir, project)
    }

    fn pg_binding(id: Uuid, owned: bool) -> ServiceBinding {
        ServiceBinding::postgres(
            DataBinding {
                branch_id: Some(id),
                database: "db".into(),
                role: "role".into(),
                url: "postgres://role@127.0.0.1:1/db".into(),
                source: "empty".into(),
                owned,
            },
            None,
        )
    }

    #[test]
    fn a_branch_being_created_is_named_with_the_engine_recorded_with_it() {
        let (_dir, project) = project(
            "\n[services.db]\nengine = \"postgres\"\n\n[services.cache]\nengine = \"valkey\"\n",
        );
        let mut receipt = Receipt::new(project.id(), "feature", None);
        let (db, cache) = (Uuid::new_v4(), Uuid::new_v4());
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: db,
            engine: Some(Engine::Postgres),
        });
        let found = named(&project, &receipt);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].engine, found[0].id, found[0].pending),
            (Some(Engine::Postgres), db, true)
        );

        receipt.creating = Some(PendingBranch {
            service: "cache".into(),
            branch_id: cache,
            engine: Some(Engine::Valkey),
        });
        let found = named(&project, &receipt);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].engine, found[0].key.as_deref()),
            (Some(Engine::Valkey), Some("cache"))
        );
    }

    #[test]
    fn the_configuration_never_decides_where_a_pending_branch_is() {
        // `db` was a PostgreSQL service when the creation began and is a Valkey
        // service now: the record says where the branch is.
        let (_dir, project) = project("\n[services.db]\nengine = \"valkey\"\n");
        let mut receipt = Receipt::new(project.id(), "feature", None);
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: Uuid::new_v4(),
            engine: Some(Engine::Postgres),
        });
        assert_eq!(named(&project, &receipt)[0].engine, Some(Engine::Postgres));
        // A record from before the engine was recorded has none, whatever the
        // configuration says today: it is looked for in every backend.
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: Uuid::new_v4(),
            engine: None,
        });
        let found = named(&project, &receipt);
        assert_eq!((found[0].engine, found[0].pending), (None, true));
    }

    #[test]
    fn a_creation_already_bound_is_named_once() {
        let (_dir, project) = project("\n[services.db]\nengine = \"postgres\"\n");
        let mut receipt = Receipt::new(project.id(), "feature", None);
        let id = Uuid::new_v4();
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: id,
            engine: Some(Engine::Postgres),
        });
        // Its credentials were recorded: the binding has it, once.
        receipt.services.insert("db".into(), pg_binding(id, true));
        let found = named(&project, &receipt);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].pending);
    }

    #[test]
    fn bindings_not_owned_are_not_named_and_the_primary_service_comes_first() {
        let (_dir, project) = project(
            "\n[services.main]\nengine = \"postgres\"\n\n[services.cache]\nengine = \"valkey\"\n",
        );
        let mut receipt = Receipt::new(project.id(), "feature", None);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        receipt.services.insert("a-old".into(), pg_binding(a, true));
        receipt.services.insert("main".into(), pg_binding(b, true));
        receipt
            .services
            .insert("attached".into(), pg_binding(Uuid::new_v4(), false));
        let found = named(&project, &receipt);
        assert_eq!(
            found.iter().map(|n| n.id).collect::<Vec<_>>(),
            [b, a],
            "{found:?}"
        );
    }

    /// A server nobody can reach, as an external one at a closed port.
    fn unreachable_server() -> (tempfile::TempDir, Project) {
        project(
            "\n[services.db]\nengine = \"postgres\"\nmode = \"external\"\n\
             admin_url = \"postgres://prov:pw@127.0.0.1:1/postgres\"\n",
        )
    }

    fn workspace_of(project: &Project, receipt: &Receipt) -> Workspace {
        let git_dir = project.root.join(".git");
        Workspace {
            name: receipt.workspace.clone(),
            path: project.root.join("w"),
            branch: None,
            is_default: false,
            locked: false,
            missing: true,
            git_dir,
            head: None,
            fingerprint: None,
        }
    }

    #[test]
    fn a_failed_drop_says_what_was_dropped_and_what_is_left() {
        let (_dir, project) = unreachable_server();
        let receipt = Receipt::new(project.id(), "feature", None);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let plan = DataPlan {
            instance: Some(receipt.instance_id),
            postgres: Server::for_project(&project).unwrap(),
            postgres_ids: vec![a, b],
            containers: Vec::new(),
        };
        assert_eq!(
            plan.describe(),
            [
                format!("database branch {a}"),
                format!("database branch {b}")
            ]
        );
        let mut warnings = Vec::new();
        let Err(failed) = execute(plan, false, &mut warnings) else {
            panic!("the server cannot be reached");
        };
        assert!(failed.done.is_empty());
        // The branch it stopped at and the one after it are both still there.
        assert_eq!(
            failed.remains,
            [
                format!("database branch {a}"),
                format!("database branch {b}")
            ]
        );
        assert!(failed.forceable);
        assert!(
            format!("{:#}", failed.cause).contains(&format!("cannot drop database branch {a}")),
            "{:#}",
            failed.cause
        );

        // Forced, each failure is a warning and nothing is left to account for.
        let plan = DataPlan {
            instance: Some(receipt.instance_id),
            postgres: Server::for_project(&project).unwrap(),
            postgres_ids: vec![a],
            containers: Vec::new(),
        };
        let (first, done) = execute(plan, true, &mut warnings).ok().unwrap();
        assert_eq!((first, done.len()), (None, 0));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(&format!("could not drop database branch {a}"))),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_branch_recorded_before_its_creation_that_cannot_be_looked_for_blocks_for_good() {
        // The server is unreachable and the engine is known: nothing can say
        // the branch does not exist, so `force` does not accept leaving it.
        let (_dir, project) = unreachable_server();
        let mut receipt = Receipt::new(project.id(), "feature", None);
        let id = Uuid::new_v4();
        receipt.creating = Some(PendingBranch {
            service: "db".into(),
            branch_id: id,
            engine: Some(Engine::Postgres),
        });
        let workspace = workspace_of(&project, &receipt);
        for force in [false, true] {
            let mut warnings = Vec::new();
            let error = plan(&project, &workspace, &receipt, force, &mut warnings)
                .err()
                .unwrap_or_else(|| panic!("force={force}"));
            let protected = error
                .downcast_ref::<crate::error::Protected>()
                .unwrap_or_else(|| panic!("{error:#}"));
            assert!(!protected.forceable, "force={force}");
            assert!(
                protected.reason.contains(&id.to_string()),
                "{}",
                protected.reason
            );
            assert!(
                !protected.reason.contains("devflow "),
                "no command: {}",
                protected.reason
            );
            let kinds: Vec<&str> = protected.remedies.iter().map(Remedy::kind).collect();
            assert_eq!(kinds, ["setup", "reconcile_databases"], "force={force}");
        }
        // A bound branch whose server cannot be reached is another matter: it
        // is accounted for by leaving it, which force accepts.
        let mut bound = Receipt::new(project.id(), "feature", None);
        bound.services.insert("db".into(), pg_binding(id, true));
        let mut warnings = Vec::new();
        let error = plan(&project, &workspace, &bound, false, &mut warnings)
            .err()
            .unwrap();
        assert!(
            error
                .downcast_ref::<crate::error::Protected>()
                .is_some_and(|p| p.forceable)
        );
        plan(&project, &workspace, &bound, true, &mut warnings).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("it was left alone")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_service_that_changed_engine_cannot_hide_a_pending_branch() {
        // The PostgreSQL creation completed, devflow crashed, and `services.db`
        // is a Valkey service now. Looking in today's backend and finding
        // nothing is not proof that nothing was made: the removal stops, for
        // a record with its engine and for one without.
        let (_dir, project) = project("\n[services.db]\nengine = \"valkey\"\n");
        for engine in [Some(Engine::Postgres), None] {
            let mut receipt = Receipt::new(project.id(), "feature", None);
            let id = Uuid::new_v4();
            receipt.creating = Some(PendingBranch {
                service: "db".into(),
                branch_id: id,
                engine,
            });
            let workspace = workspace_of(&project, &receipt);
            for force in [false, true] {
                let mut warnings = Vec::new();
                let error = plan(&project, &workspace, &receipt, force, &mut warnings)
                    .err()
                    .unwrap_or_else(|| panic!("{engine:?} force={force}: accepted as absent"));
                let protected = error
                    .downcast_ref::<crate::error::Protected>()
                    .unwrap_or_else(|| panic!("{error:#}"));
                assert!(!protected.forceable, "{engine:?} force={force}");
                assert!(
                    protected.reason.contains(&id.to_string())
                        && protected.reason.contains("no postgres service any more"),
                    "{}",
                    protected.reason
                );
            }
        }
    }
}
