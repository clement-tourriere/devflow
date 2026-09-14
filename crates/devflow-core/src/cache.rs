//! Explicit, fingerprinted artifact reuse. Database generations never use this path.
use anyhow::{Context, Result};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    #[default]
    Copy,
    SharedReadOnly,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheSpec {
    pub path: String,
    pub inputs: Vec<String>,
    #[serde(default)]
    pub policy: CachePolicy,
    #[serde(default = "default_budget")]
    pub max_bytes: u64,
}
fn default_budget() -> u64 {
    5 * 1024 * 1024 * 1024
}
#[derive(Debug, Clone, Serialize)]
pub struct CacheResult {
    pub path: String,
    pub fingerprint: String,
    pub policy: CachePolicy,
    pub bytes: u64,
    pub reused: bool,
    /// A cache miss permits the workspace's normal build to proceed.
    pub miss_reason: Option<String>,
}
fn relative(path: &Path) -> Result<()> {
    anyhow::ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "cache paths must be relative without '.' or '..'"
    );
    anyhow::ensure!(
        !path.components().any(|c| matches!(
            c.as_os_str().to_str(),
            Some(".git" | ".jj" | ".agents" | ".codex" | ".claude")
        )),
        "VCS and agent policy directories are not cache artifacts"
    );
    Ok(())
}
fn fingerprint(root: &Dir, spec: &CacheSpec) -> Result<String> {
    use sha2::{Digest, Sha256};
    anyhow::ensure!(
        !spec.inputs.is_empty(),
        "cache '{}' needs explicit fingerprint inputs",
        spec.path
    );
    let mut hash = Sha256::new();
    hash.update(spec.path.as_bytes());
    for input in &spec.inputs {
        relative(Path::new(input))?;
        hash.update((input.len() as u64).to_be_bytes());
        hash.update(input.as_bytes());
        let mut file = root
            .open(input)
            .with_context(|| format!("cache fingerprint input {input} is missing"))?;
        hash.update(file.metadata()?.len().to_be_bytes());
        let mut data = [0u8; 65536];
        loop {
            let n = file.read(&mut data)?;
            if n == 0 {
                break;
            }
            hash.update(&data[..n]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn copy_tree(
    source: &Dir,
    from: &Path,
    dest: &Dir,
    to: &Path,
    readonly: bool,
    remaining: &mut u64,
    depth: usize,
) -> Result<()> {
    anyhow::ensure!(
        depth < 128,
        "cache directory nesting exceeds 128 levels (possible symlink cycle)"
    );
    let meta = source.symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        anyhow::ensure!(depth > 0, "cache artifact root must not be a symlink");
        let target = source.read_link_contents(from)?;
        let mut levels = depth - 1;
        for component in target.components() {
            match component {
                Component::Normal(_) => levels += 1,
                Component::CurDir => (),
                Component::ParentDir if levels > 0 => levels -= 1,
                _ => anyhow::bail!("cache symlink escapes its artifact: {}", from.display()),
            }
        }
        if let Some(parent) = to.parent() {
            dest.create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            dest.symlink_contents(target, to)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        anyhow::bail!("cache symlink preservation requires Unix");
    }
    if meta.is_dir() {
        dest.create_dir_all(to)?;
        for entry in source.read_dir(from)? {
            let entry = entry?;
            copy_tree(
                source,
                &from.join(entry.file_name()),
                dest,
                &to.join(entry.file_name()),
                readonly,
                remaining,
                depth + 1,
            )?;
        }
    } else {
        anyhow::ensure!(meta.is_file(), "cache contains a non-regular file");
        anyhow::ensure!(
            meta.len() <= *remaining,
            "cache exceeds its configured max_bytes budget"
        );
        if let Some(parent) = to.parent() {
            dest.create_dir_all(parent)?;
        }
        let input = source.open(from)?;
        let mut input = input.take(remaining.saturating_add(1));
        let mut output = dest.create(to)?;
        let copied = std::io::copy(&mut input, &mut output)?;
        anyhow::ensure!(
            copied <= *remaining,
            "cache grew beyond its configured max_bytes budget"
        );
        *remaining -= copied;
        output.flush()?;
    }
    let mut permissions = meta.permissions();
    if readonly {
        permissions.set_readonly(true);
    }
    dest.set_permissions(to, permissions)?;
    Ok(())
}

// Only owns unpublished UUID staging paths. Committed paths have moved away.
struct StagingTree(PathBuf);
impl Drop for StagingTree {
    fn drop(&mut self) {
        if let (Some(parent), Some(name)) = (self.0.parent(), self.0.file_name()) {
            if let Ok(dir) = Dir::open_ambient_dir(parent, cap_std::ambient_authority()) {
                let _ = make_writable(&dir, Path::new(name));
                if self.0.is_dir() {
                    let _ = dir.remove_dir_all(name);
                } else {
                    let _ = dir.remove_file(name);
                }
            }
        }
    }
}

pub fn materialize(
    config: &crate::config::Config,
    source: &Path,
    target: &Path,
) -> Result<Vec<CacheResult>> {
    if config.worktree.caches.is_empty() || source.canonicalize()? == target.canonicalize()? {
        return Ok(Vec::new());
    }
    let input = Dir::open_ambient_dir(source, cap_std::ambient_authority())?;
    let output = Dir::open_ambient_dir(target, cap_std::ambient_authority())?;
    let identity = crate::state::LocalStateManager::new()?.control(source).id;
    let mut results = Vec::new();
    for spec in &config.worktree.caches {
        relative(Path::new(&spec.path))?;
        let desired = fingerprint(&output, spec);
        let source_fingerprint = fingerprint(&input, spec);
        let (fingerprint, source_matches) = match (desired, source_fingerprint) {
            (Ok(desired), Ok(source)) => (desired.clone(), desired == source),
            (Ok(desired), Err(error))
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                (desired, false)
            }
            (Err(error), _) | (_, Err(error)) => {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_none_or(|e| e.kind() != std::io::ErrorKind::NotFound)
                {
                    return Err(error);
                }
                results.push(CacheResult {
                    path: spec.path.clone(),
                    fingerprint: String::new(),
                    policy: spec.policy,
                    bytes: 0,
                    reused: false,
                    miss_reason: Some("fingerprint input is missing".into()),
                });
                continue;
            }
        };
        let root = crate::paths::devflow_config_dir()?
            .join("caches")
            .join(&identity.0)
            .join(&fingerprint);
        let _lease =
            crate::services::ownership::OperationLease::acquire_sync(&root.with_extension("lock"))?;
        let artifact = root.join("artifact");
        let receipt = root.join("receipt.json");
        let reused = receipt.is_file();
        if !reused && (!source_matches || !input.try_exists(&spec.path)?) {
            results.push(CacheResult {
                path: spec.path.clone(),
                fingerprint,
                policy: spec.policy,
                bytes: 0,
                reused: false,
                miss_reason: Some("no artifact matches the destination inputs".into()),
            });
            continue;
        }
        let bytes = if reused {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&receipt)?)?["bytes"]
                .as_u64()
                .context("invalid cache receipt")?
        } else {
            // Only publish complete artifacts; abandoned staging trees are never reused.
            let stage = root.with_extension(format!("stage-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&stage)?;
            let _staging = StagingTree(stage.clone());
            let stage_dir = Dir::open_ambient_dir(&stage, cap_std::ambient_authority())?;
            let mut remaining = spec.max_bytes;
            copy_tree(
                &input,
                Path::new(&spec.path),
                &stage_dir,
                Path::new("artifact"),
                true,
                &mut remaining,
                0,
            )?;
            let bytes = spec.max_bytes - remaining;
            crate::services::ownership::atomic_write(
                &stage.join("receipt.json"),
                &serde_json::to_vec(
                    &serde_json::json!({"schema_version":1,"fingerprint":fingerprint,"bytes":bytes}),
                )?,
            )?;
            std::fs::rename(&stage, &root)?;
            bytes
        };
        anyhow::ensure!(
            bytes <= spec.max_bytes,
            "reused cache exceeds the configured budget"
        );
        let destination = Path::new(&spec.path);
        let binding = crate::paths::devflow_config_dir()?
            .join("cache-bindings")
            .join(&identity.0)
            .join(format!(
                "{}.json",
                crate::services::ownership::identity_digest(&[
                    &target.canonicalize()?.to_string_lossy(),
                    &spec.path,
                ])
            ));
        if output.symlink_metadata(destination).is_ok() {
            // Preserve mutable workspace copies only when their recorded inputs match.
            if spec.policy == CachePolicy::SharedReadOnly {
                anyhow::ensure!(target.join(destination).canonicalize()?==artifact.canonicalize()?,"workspace has a different cache generation at {}; remove it explicitly before reuse",spec.path);
            } else {
                let previous = std::fs::read(&binding)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
                anyhow::ensure!(previous.as_ref().is_some_and(|r| r["fingerprint"] == fingerprint && r["policy"] == "copy")
                    && !output.symlink_metadata(destination)?.file_type().is_symlink(),
                    "workspace cache generation is different or untracked at {}; remove it explicitly before reuse", spec.path);
            }
        } else {
            match spec.policy {
                CachePolicy::Copy => {
                    let source = Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
                    let mut remaining = spec.max_bytes;
                    let stage = PathBuf::from(format!(".devflow-cache-{}", uuid::Uuid::new_v4()));
                    let _staging = StagingTree(target.join(&stage));
                    copy_tree(
                        &source,
                        Path::new("artifact"),
                        &output,
                        &stage,
                        false,
                        &mut remaining,
                        0,
                    )?;
                    make_writable(&output, &stage)?;
                    if let Some(parent) = destination.parent() {
                        output.create_dir_all(parent)?;
                    }
                    output.rename(&stage, &output, destination)?;
                }
                CachePolicy::SharedReadOnly => {
                    if let Some(parent) = destination.parent() {
                        output.create_dir_all(parent)?;
                    }
                    #[cfg(unix)]
                    output.symlink_contents(&artifact, destination)?;
                    #[cfg(not(unix))]
                    anyhow::bail!(
                        "shared_read_only caches require Unix; use copy on this platform"
                    );
                }
            }
            crate::services::ownership::atomic_write(
                &binding,
                &serde_json::to_vec(
                    &serde_json::json!({"schema_version":1,"fingerprint":fingerprint,"policy":spec.policy}),
                )?,
            )?;
        }
        results.push(CacheResult {
            path: spec.path.clone(),
            fingerprint,
            policy: spec.policy,
            bytes,
            reused,
            miss_reason: None,
        });
    }
    Ok(results)
}
fn make_writable(dir: &Dir, path: &Path) -> Result<()> {
    let meta = dir.symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut permissions = meta.permissions();
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o200 | if meta.is_dir() { 0o100 } else { 0 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    dir.set_permissions(path, permissions)?;
    if meta.is_dir() {
        for entry in dir.read_dir(path)? {
            make_writable(dir, &path.join(entry?.file_name()))?;
        }
    }
    Ok(())
}
