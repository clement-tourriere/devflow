//! Immutable PostgreSQL data generations, independent of Git workspaces.
use crate::{
    config::Config,
    services::{
        factory,
        ownership::{identity_digest, OperationLease},
    },
    state::{LocalStateManager, ResourceId, ServiceId},
    workspace::operation::OperationJournal,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataOrigin {
    Workspace {
        workspace: String,
        service_key: String,
    },
    Baseline {
        reference: String,
        service_key: String,
        generation: String,
        engine_fingerprint: String,
    },
    Empty,
}
impl DataOrigin {
    pub fn service_key(&self) -> Option<&str> {
        match self {
            Self::Workspace { service_key, .. } | Self::Baseline { service_key, .. } => {
                Some(service_key)
            }
            Self::Empty => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub reference: String,
    pub name: String,
    pub version: u64,
    pub service: String,
    pub service_id: ServiceId,
    pub resource_id: ResourceId,
    pub service_key: String,
    pub source_fingerprint: String,
    pub engine_fingerprint: Option<String>,
    pub status: String,
    pub operation_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub fn source_fingerprint(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let len = file.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn list(project: &Path) -> Result<Vec<Baseline>> {
    Ok(LocalStateManager::new()?
        .control(project)
        .baselines
        .into_values()
        .flatten()
        .collect())
}

pub fn resolve(project: &Path, reference: &str) -> Result<Baseline> {
    anyhow::ensure!(
        reference.starts_with("baseline:") && reference.contains("@v"),
        "use an exact baseline reference: baseline:NAME@vVERSION"
    );
    let baseline = list(project)?
        .into_iter()
        .find(|b| b.reference == reference)
        .context("baseline version not found")?;
    anyhow::ensure!(
        baseline.status == "sealed",
        "baseline is not sealed; inspect operation {}",
        baseline.operation_id
    );
    Ok(baseline)
}

/// Inputs are local dump/SQL files. Each request creates a new immutable version.
/// A failed preparation remains visible and may be explicitly removed.
pub async fn prepare(
    config: &Config,
    project: &Path,
    name: &str,
    service: &str,
    source: &Path,
    migrate: Option<&Path>,
    validate: Option<&Path>,
) -> Result<Baseline> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "baseline name must contain only letters, digits, '-' or '_'"
    );
    let provider = factory::resolve_provider(config, Some(service)).await?;
    anyhow::ensure!(
        provider.provider.operations().snapshot && provider.provider.operations().seed,
        "baselines require the local PostgreSQL provider"
    );
    let source = source.canonicalize()?;
    let migrate = migrate.map(Path::canonicalize).transpose()?;
    let validate = validate.map(Path::canonicalize).transpose()?;
    let snapshot = InputSnapshot::new([
        Some(source.as_path()),
        migrate.as_deref(),
        validate.as_deref(),
    ])?;
    let fingerprints = snapshot
        .files
        .iter()
        .flatten()
        .map(|path| source_fingerprint(path))
        .collect::<Result<Vec<_>>>()?;
    let fingerprint = identity_digest(&fingerprints.iter().map(String::as_str).collect::<Vec<_>>());
    let control = LocalStateManager::new()?.control(project);
    let _lease = OperationLease::acquire(
        &crate::paths::devflow_config_dir()?
            .join("baseline-locks")
            .join(format!("{}.lock", control.id.0)),
    )
    .await?;
    let version = LocalStateManager::new()?.update_control(project, |control| {
        let existing = control
            .baselines
            .get(name)
            .into_iter()
            .flatten()
            .map(|b| b.version)
            .max()
            .unwrap_or(0);
        let counter = control.baseline_versions.entry(name.into()).or_default();
        *counter = (*counter)
            .max(existing)
            .checked_add(1)
            .context("baseline version space exhausted")?;
        Ok(*counter)
    })?;
    let reference = format!("baseline:{name}@v{version}");
    let service_key = format!(
        "dfbase_{}",
        &identity_digest(&[&control.id.0, service, &reference])[..32]
    );
    let mut journal =
        OperationJournal::begin(project, &reference, "baseline", None, None, true).await?;
    journal.set_request(serde_json::json!({"name":name,"service":service,"source":source,"migrate":migrate,"validate":validate}), config)?;
    let mut baseline = Baseline {
        reference,
        name: name.into(),
        version,
        service: service.into(),
        service_id: ServiceId(identity_digest(&[&control.id.0, "service", service])),
        resource_id: ResourceId(identity_digest(&[&control.id.0, service, &service_key])),
        service_key,
        source_fingerprint: fingerprint,
        engine_fingerprint: None,
        status: "preparing".into(),
        operation_id: journal.record.id.clone(),
        created_at: chrono::Utc::now(),
    };
    persist(project, &baseline)?;
    let result: Result<()> = async {
        journal.phase("create")?;
        provider
            .provider
            .create_workspace(&baseline.service_key, None)
            .await?;
        journal.completed("create")?;
        for (phase, path) in [
            ("seed", snapshot.files[0].as_deref()),
            ("migrate", snapshot.files[1].as_deref()),
            ("validate", snapshot.files[2].as_deref()),
        ] {
            if let Some(path) = path {
                journal.phase(phase)?;
                provider
                    .provider
                    .seed_from_source(&baseline.service_key, &path.display().to_string())
                    .await?;
                journal.completed(phase)?;
            }
        }
        journal.phase("seal")?;
        baseline.engine_fingerprint = Some(
            provider
                .provider
                .seal_baseline(&baseline.service_key)
                .await?,
        );
        baseline.status = "sealed".into();
        persist(project, &baseline)?;
        journal.completed("seal")?;
        journal.finish(true)?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        baseline.status = "failed".into();
        persist(project, &baseline)?;
        return Err(journal.fail(error));
    }
    Ok(baseline)
}

fn persist(project: &Path, baseline: &Baseline) -> Result<()> {
    LocalStateManager::new()?.update_control(project, |control| {
        let versions = control.baselines.entry(baseline.name.clone()).or_default();
        if let Some(existing) = versions.iter_mut().find(|v| v.version == baseline.version) {
            *existing = baseline.clone();
        } else {
            versions.push(baseline.clone());
        }
        Ok(())
    })
}

pub async fn remove(config: &Config, project: &Path, reference: &str) -> Result<()> {
    let control = LocalStateManager::new()?.control(project);
    let _lease = OperationLease::acquire(
        &crate::paths::devflow_config_dir()?
            .join("baseline-locks")
            .join(format!("{}.lock", control.id.0)),
    )
    .await?;
    let mut journal =
        OperationJournal::begin(project, reference, "baseline-remove", None, None, false).await?;
    let control = LocalStateManager::new()?.control(project);
    let baseline = control
        .baselines
        .values()
        .flatten()
        .find(|b| b.reference == reference)
        .context("baseline not found")?;
    anyhow::ensure!(
        !control.workspaces.values().any(|w| w.desired
            != crate::state::control::DesiredState::Absent
            && w.data
                .values()
                .any(|d| matches!(d,DataOrigin::Baseline { reference:r,.. } if r == reference))),
        "baseline has dependent workspaces"
    );
    let provider = factory::resolve_provider(config, Some(&baseline.service)).await?;
    journal.set_request(serde_json::json!({"reference":reference}), config)?;
    if let Err(error) = provider
        .provider
        .delete_baseline(&baseline.service_key)
        .await
    {
        return Err(journal.fail(error));
    }
    LocalStateManager::new()?.update_control(project, |control| {
        for versions in control.baselines.values_mut() {
            versions.retain(|b| b.reference != reference);
        }
        Ok(())
    })?;
    journal.finish(true)
}

/// Resolve once and persist exact per-service origins before any provider mutation.
pub fn bind_origins(
    config: &Config,
    project: &Path,
    workspace: &str,
    parent: Option<&str>,
) -> Result<BTreeMap<String, DataOrigin>> {
    let state = LocalStateManager::new()?;
    let baseline = parent
        .filter(|p| p.starts_with("baseline:"))
        .map(|p| resolve(project, p))
        .transpose()?;
    LocalStateManager::new()?.update_control(project, |control| {
        let binding = control.workspace(workspace);
        for service in config
            .resolve_services()
            .iter()
            .filter(|s| s.auto_workspace)
        {
            if binding.data.contains_key(&service.name) {
                continue;
            }
            let origin = if let Some(baseline) = &baseline {
                if baseline.service == service.name {
                    DataOrigin::Baseline {
                        reference: baseline.reference.clone(),
                        service_key: baseline.service_key.clone(),
                        generation: baseline.resource_id.0.clone(),
                        engine_fingerprint: baseline
                            .engine_fingerprint
                            .clone()
                            .context("sealed baseline lacks an engine fingerprint")?,
                    }
                } else {
                    DataOrigin::Empty
                }
            } else if !matches!(
                service.service_type.as_str(),
                "postgres" | "mysql" | "mariadb" | "clickhouse"
            ) || (service.service_type == "clickhouse"
                && matches!(service.provider_type.as_str(), "shared" | "global"))
            {
                DataOrigin::Empty
            } else if let Some(parent) = parent {
                DataOrigin::Workspace {
                    workspace: parent.into(),
                    service_key: state.resolve_workspace_service_key_by_dir(project, parent)?,
                }
            } else {
                DataOrigin::Empty
            };
            binding.data.insert(service.name.clone(), origin);
        }
        if let Some(baseline) = &baseline {
            anyhow::ensure!(
                binding.data.contains_key(&baseline.service),
                "baseline service must be configured with auto_workspace: true"
            );
        }
        Ok(binding.data.clone())
    })
}

/// Import the exact bytes that were fingerprinted, even if an input changes later.
struct InputSnapshot {
    root: PathBuf,
    files: [Option<PathBuf>; 3],
}
impl InputSnapshot {
    fn new(inputs: [Option<&Path>; 3]) -> Result<Self> {
        let root = crate::paths::devflow_config_dir()?
            .join("baseline-inputs")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root)?;
        let mut snapshot = Self {
            root,
            files: Default::default(),
        };
        for (index, path) in inputs.into_iter().enumerate() {
            if let Some(path) = path {
                let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("dump");
                let destination = snapshot.root.join(format!("{index}.{extension}"));
                let mut input = std::fs::File::open(path)?;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut output = options.open(&destination)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                snapshot.files[index] = Some(destination);
            }
        }
        Ok(snapshot)
    }
}
impl Drop for InputSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
