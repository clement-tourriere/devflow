//! Free text from core, masked once, where a view model is built.
//!
//! A successful call can still return text that quotes a credential: a
//! pitchfork listing with the URL a daemon failed to connect to, a server
//! problem that echoes its connection string, a branch note. `jobs::catch`
//! masks failures; this module masks the strings inside successful results,
//! one function per model, so no view has to remember to.
use crate::model::DatabaseModel;
use devflow_core::{
    pitchfork::{Detection, ProxyStatus},
    processes::{Gate, ProcessStatus, Refusal},
    redact,
};

fn mask(text: &mut String) {
    let masked = redact::text(text);
    if masked != *text {
        *text = masked;
    }
}

fn mask_option(text: &mut Option<String>) {
    if let Some(text) = text {
        mask(text);
    }
}

fn mask_all(texts: &mut [String]) {
    texts.iter_mut().for_each(mask);
}

/// The one way a workspace, branch, daemon, snapshot or service name is drawn:
/// credentials masked, everything else as it is. Those names are legal even
/// when they look like a credential (`password=hunter2`), so they are
/// identities for requests (kept raw, see [`RawName`]) and only their label is
/// masked, here, where it is drawn.
pub fn shown(name: &str) -> String {
    redact::text(name)
}

/// A name exactly as core recorded it: the identity an operation acts on
/// (`DataSource::Snapshot { name, .. }`), never text. It has no `Display` and
/// its `Debug` prints nothing, so a view cannot draw it by accident; `shown`
/// is the masked text a label may use. Masking a name in place would turn
/// `password=hunter2` into `password=***` and make the operation target a
/// snapshot that does not exist.
#[derive(Clone, PartialEq, Eq)]
pub struct RawName(String);

impl RawName {
    /// The recorded name, for the request that names the snapshot.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name as a label shows it: credentials masked.
    pub fn shown(&self) -> String {
        shown(&self.0)
    }
}

impl std::fmt::Debug for RawName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RawName(..)")
    }
}

impl From<&str> for RawName {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

impl From<String> for RawName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

/// What pitchfork was found and how it can be installed.
pub fn detection(detection: &mut Detection) {
    mask_option(&mut detection.version);
    mask_option(&mut detection.problem);
    mask_all(&mut detection.missing_features);
}

/// The proxy's routes: the URLs and states pitchfork prints.
pub fn proxy(proxy: &mut ProxyStatus) {
    mask_option(&mut proxy.scheme);
    mask_option(&mut proxy.tld);
    for project in &mut proxy.projects {
        mask(&mut project.project);
        for daemon in &mut project.daemons {
            mask(&mut daemon.daemon);
            mask_option(&mut daemon.url);
            mask_option(&mut daemon.status);
        }
    }
}

/// Everything core said about a workspace's processes.
pub fn process_status(status: &mut ProcessStatus) {
    mask(&mut status.workspace);
    mask(&mut status.namespace.value);
    mask_option(&mut status.version);
    mask_option(&mut status.error);
    mask_option(&mut status.autostart_blocked);
    mask_all(&mut status.observation.reasons);
    for daemon in &mut status.daemons {
        // `name` and `id` are the identity a start asks core for: a daemon
        // called `password=hunter2` masked to `password=***` would name
        // another daemon. They stay as pitchfork has them and are masked where
        // they are drawn (`shown`).
        mask(&mut daemon.state);
        mask_option(&mut daemon.url);
        mask_option(&mut daemon.error);
        mask_all(&mut daemon.triggers);
    }
    if let Some(found) = &mut status.proxy {
        proxy(found);
    }
    gate(&mut status.can_start);
    gate(&mut status.can_stop);
}

fn gate(gate: &mut Gate) {
    mask_option(&mut gate.reason);
    if let Some(refusal) = &mut gate.refusal {
        refusal_in_place(refusal);
    }
}

/// The text fields of the refusals core has today. A kind it adds later is
/// still masked where it is rendered; only its structured payload stays as
/// core made it.
fn refusal_in_place(refusal: &mut Refusal) {
    match refusal {
        Refusal::NotInstalled { detail } => mask(detail),
        Refusal::Unreadable { file, problem } => {
            mask(file);
            mask(problem);
        }
        Refusal::NotReady { status, detail } => {
            mask(status);
            mask_option(detail);
        }
        Refusal::AutonomousTriggers { triggers } => {
            for trigger in triggers {
                mask(&mut trigger.file);
                mask(&mut trigger.daemon);
                mask(&mut trigger.trigger);
            }
        }
        Refusal::UnknownDaemon { name } => mask(name),
        Refusal::MiseDelivery { daemons, .. } => mask_all(daemons),
        _ => {}
    }
}

/// The PostgreSQL server, its branches and snapshots.
pub fn database(db: &mut DatabaseModel) {
    mask_option(&mut db.error);
    if let Some(status) = &mut db.status {
        mask_option(&mut status.endpoint);
        mask_option(&mut status.container_endpoint);
        mask_option(&mut status.server_version);
        mask_option(&mut status.file_copy_method);
        mask_option(&mut status.problem);
    }
    // Branch and snapshot names are identities (a snapshot is deleted or seeds
    // a workspace by its name): they stay as recorded and are masked where
    // they are drawn.
    for branch in &mut db.branches {
        mask_option(&mut branch.error);
        mask_all(&mut branch.warnings);
    }
    for snapshot in &mut db.snapshots {
        mask_all(&mut snapshot.warnings);
    }
}

/// The container services: every name and note core or Docker produced.
pub fn services(model: &mut crate::services::ServicesModel) {
    mask_option(&mut model.error);
    // `orphans.services` holds service keys, the identity of a forget: raw.
    mask_all(&mut model.orphans.incomplete);
    for orphan in &mut model.orphans.branches {
        mask(&mut orphan.name);
        mask(&mut orphan.reason);
    }
    for detail in model.details.iter_mut().flatten() {
        mask(&mut detail.image);
        mask_option(&mut detail.error);
        mask_option(&mut detail.cow.note);
        if let Some(last) = &mut detail.last_clone {
            // What is drawn; `target_id` is the identity.
            mask(&mut last.operation);
            mask(&mut last.target);
            mask(&mut last.finished);
            mask_option(&mut last.method);
        }
        for branch in &mut detail.branches {
            mask(&mut branch.name);
            mask(&mut branch.source);
            mask_option(&mut branch.endpoint);
            mask_option(&mut branch.container_endpoint);
            mask_option(&mut branch.masked_url);
        }
        for snapshot in &mut detail.snapshots {
            // `name` is an identity (a `RawName`): it stays as core recorded
            // it and is masked where it is shown.
            mask(&mut snapshot.source);
        }
    }
}

/// A typed refusal as the views show it: a stable word, the sentence and what
/// the person can do. Nothing else of the refusal is read, so a new kind in
/// core renders without a change here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefusalView {
    /// `Refusal::code`: `mise_delivery`, `not_approved`, …
    pub code: String,
    /// `Display`, masked.
    pub text: String,
    /// `Refusal::hint`, masked.
    pub hint: Option<String>,
}

impl RefusalView {
    pub fn of(refusal: &Refusal) -> Self {
        Self {
            code: refusal.code().to_string(),
            text: redact::text(&refusal.to_string()),
            hint: refusal.hint().map(|hint| redact::text(&hint)),
        }
    }

    /// The refusal that explains a start that did not happen, if the error
    /// is one.
    pub fn from_error(error: &anyhow::Error) -> Option<Self> {
        error.downcast_ref::<Refusal>().map(Self::of)
    }

    /// Is this the refusal of the mise delivery check?
    pub fn is_mise_delivery(&self) -> bool {
        self.code == "mise_delivery"
    }

    /// Is this a state core could not read, the refusal a forced stop answers?
    pub fn is_state_unknown(&self) -> bool {
        self.code == "state_unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devflow_core::{
        pitchfork::ProxyProject,
        processes::{DaemonState, Namespace, NamespaceSource, Observation},
    };
    use std::path::PathBuf;

    // kingfisher:ignore
    const URL: &str = "postgres://u:review-password@localhost/db";
    const SECRET: &str = "review-password";

    fn status() -> ProcessStatus {
        let refused = |refusal: Refusal| Gate {
            allowed: false,
            reason: Some(refusal.to_string()),
            refusal: Some(refusal),
        };
        ProcessStatus {
            workspace: "feature".into(),
            path: PathBuf::from("/tmp/shop/feature"),
            instance: None,
            namespace: Namespace {
                value: "df-0b3c9d2e-a1b2c3d4".into(),
                source: NamespaceSource::Devflow,
            },
            installed: true,
            version: Some("2.29.0".into()),
            configured: true,
            supervisor_up: Some(true),
            daemons: vec![DaemonState {
                name: "web".into(),
                id: Some("df-0b3c9d2e-a1b2c3d4/web".into()),
                state: "errored".into(),
                pid: None,
                ports: vec![3000],
                url: Some("https://web.localhost".into()),
                error: Some(format!("cannot connect to {URL}")),
                triggers: Vec::new(),
                configured: true,
                startable: false,
            }],
            observation: Observation::from_reasons(vec![format!("cannot connect to {URL}")]),
            proxy: Some(ProxyStatus {
                enabled: true,
                scheme: Some("https".into()),
                tld: Some("localhost".into()),
                port: Some(443),
                trusted: Some(true),
                projects: vec![ProxyProject {
                    project: "df-0b3c9d2e-a1b2c3d4".into(),
                    dir: PathBuf::from("/tmp/shop/feature"),
                    daemons: vec![devflow_core::pitchfork::ProxyDaemon {
                        daemon: "web".into(),
                        url: Some(format!("https://web.localhost/?next={URL}")),
                        status: Some("running".into()),
                        port: Some(3000),
                    }],
                }],
            }),
            error: Some(format!("pitchfork failed: {URL}")),
            can_start: refused(Refusal::NotReady {
                status: "needs_attention".into(),
                detail: Some(format!("receipt says {URL}")),
            }),
            can_stop: refused(Refusal::NotInstalled {
                detail: format!("{URL} is not pitchfork"),
            }),
            autostart: true,
            autostart_blocked: Some(format!("blocked by {URL}")),
        }
    }

    #[test]
    fn a_credential_in_a_successful_status_is_masked_everywhere() {
        let mut status = status();
        assert!(format!("{status:?}").contains(SECRET), "the fixture leaks");
        process_status(&mut status);
        let shown = format!("{status:?}");
        assert!(!shown.contains(SECRET), "{shown}");
        // The rest of the text is still there.
        assert!(shown.contains("localhost/db") && shown.contains("cannot connect to"));
        assert!(status.can_start.reason.as_deref().unwrap().contains("***"));
        // The structure the buttons read is untouched.
        assert!(matches!(
            status.can_start.refusal,
            Some(Refusal::NotReady { .. })
        ));
        assert_eq!(status.daemons[0].ports, [3000]);
    }

    #[test]
    fn a_name_an_action_is_requested_by_is_never_masked_in_the_model() {
        use crate::services::{OrphanBranch, ServicesModel};
        use chrono::Utc;
        use devflow_core::db::{RecordStatus, SnapshotInfo};
        let name = "password=hunter2";
        // A daemon, by name and by pitchfork's id.
        let mut status = status();
        status.daemons[0].name = name.into();
        status.daemons[0].id = Some(format!("df-0b3c9d2e-a1b2c3d4/{name}"));
        process_status(&mut status);
        assert_eq!(status.daemons[0].name, name);
        assert_eq!(
            status.daemons[0].id.as_deref(),
            Some("df-0b3c9d2e-a1b2c3d4/password=hunter2")
        );
        // A PostgreSQL snapshot (deleted and used as a data source by name).
        let mut db = DatabaseModel {
            snapshots: vec![SnapshotInfo {
                id: uuid::Uuid::new_v4(),
                name: name.into(),
                version: 1,
                db_name: "df_seed".into(),
                source_branch_id: None,
                status: RecordStatus::Ready,
                created_at: Utc::now(),
                warnings: Vec::new(),
            }],
            ..DatabaseModel::default()
        };
        database(&mut db);
        assert_eq!(db.snapshots[0].name, name);
        let options = crate::model::data_options("default", &[], &db.snapshots);
        let snapshot = options.last().unwrap();
        assert_eq!(snapshot.label, "Snapshot password=***@v1");
        assert!(matches!(
            &snapshot.source,
            Some(devflow_core::db::DataSource::Snapshot { name: n, .. }) if n == name
        ));
        // A service key the catalog has and the configuration does not (what
        // a forget names).
        let mut model = ServicesModel::default();
        model.orphans.services = vec![name.into()];
        model.orphans.branches = vec![OrphanBranch {
            service: "cache".into(),
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            reason: "no workspace".into(),
        }];
        services(&mut model);
        assert_eq!(model.orphans.services, [name]);
        // Only the label of an orphan branch is masked; its id is the identity.
        assert_eq!(model.orphans.branches[0].name, "password=***");
        assert_eq!(shown(name), "password=***");
        assert_eq!(shown("feature-x"), "feature-x");
    }

    #[test]
    fn detection_and_proxy_are_masked_too() {
        let mut found = Detection {
            path: Some(PathBuf::from("/usr/local/bin/pitchfork")),
            installed: false,
            version: None,
            missing_features: vec![format!("list --json ({URL})")],
            problem: Some(format!("exit 1: {URL}")),
        };
        detection(&mut found);
        let shown = format!("{found:?} {}", found.summary());
        assert!(!shown.contains(SECRET), "{shown}");
    }

    #[test]
    fn a_refusal_renders_as_its_kind_its_sentence_and_its_hint() {
        let refusal = Refusal::NotInstalled {
            detail: format!("pitchfork broke: {URL}"),
        };
        let view = RefusalView::of(&refusal);
        assert_eq!(view.code, "not_installed");
        assert!(!view.text.contains(SECRET) && view.text.contains("pitchfork broke"));
        assert_eq!(view.hint.as_deref(), refusal.hint().as_deref());
        assert!(!view.is_mise_delivery());
        let mise = Refusal::MiseDelivery {
            daemons: vec!["web".into()],
            delivery: devflow_core::env_file::MiseDelivery::Failed {
                reason: format!("mise said {URL}"),
            },
        };
        let view = RefusalView::of(&mise);
        assert!(view.is_mise_delivery());
        assert!(!view.text.contains(SECRET), "{}", view.text);
        let error = anyhow::Error::new(mise).context("starting");
        assert_eq!(RefusalView::from_error(&error), Some(view));
        assert_eq!(RefusalView::from_error(&anyhow::anyhow!("other")), None);
    }

    #[test]
    fn database_notes_and_problems_are_masked() {
        use chrono::Utc;
        use devflow_core::{
            config::{CowCapability, PostgresMode},
            db::{BranchInfo, RecordStatus, ServerStatus, SnapshotInfo},
        };
        let mut db = DatabaseModel {
            configured: true,
            status: Some(ServerStatus {
                mode: PostgresMode::External,
                running: true,
                reachable: false,
                endpoint: Some(URL.into()),
                container_endpoint: None,
                server_version: None,
                file_copy_method: None,
                cow: CowCapability::Unknown,
                cow_source: None,
                problem: Some(format!("cannot reach {URL}")),
            }),
            branches: vec![BranchInfo {
                id: uuid::Uuid::new_v4(),
                name: "feature".into(),
                db_name: "df_feature".into(),
                role_name: "dfl_x".into(),
                source_kind: "empty".into(),
                source_id: None,
                instance_id: None,
                status: RecordStatus::Failed,
                cow: CowCapability::Unknown,
                error: Some(format!("failed: {URL}")),
                created_at: Utc::now(),
                size_bytes: None,
                generation: 0,
                extras: Vec::new(),
                warnings: vec![format!("note {URL}")],
            }],
            snapshots: vec![SnapshotInfo {
                id: uuid::Uuid::new_v4(),
                name: "seed".into(),
                version: 1,
                db_name: "df_seed".into(),
                source_branch_id: None,
                status: RecordStatus::Ready,
                created_at: Utc::now(),
                warnings: vec![format!("note {URL}")],
            }],
            error: Some(format!("list failed: {URL}")),
        };
        database(&mut db);
        let shown = format!("{db:?}");
        assert!(!shown.contains(SECRET), "{shown}");
    }
}
