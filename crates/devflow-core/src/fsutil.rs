//! Small filesystem helpers shared by state writers.
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

/// Write `content` to `path` atomically: a sibling temporary file is fully
/// written and synced, then renamed over the target. `private` restricts the
/// file to the current user (credentials, receipts).
pub(crate) fn write_atomic(path: &Path, content: &[u8], private: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;
    let name = path
        .file_name()
        .context("path has no file name")?
        .to_string_lossy();
    let tmp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&tmp)?;
        file.write_all(content)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("cannot write {}", path.display()))
}

/// Run a read-modify-write of a shared state file under an exclusive lock on
/// `lock_path`, so concurrent writers (two terminals, the GUI) don't lose each
/// other's updates. Readers need no lock: writes replace files atomically.
pub(crate) fn with_lock<T>(lock_path: &Path, change: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("cannot lock {}", lock_path.display()))?;
    change()
}
