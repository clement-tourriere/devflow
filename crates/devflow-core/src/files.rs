//! Capability-scoped file access for hooks, generated files, and config editing.
//! All traversal is relative to an open directory, including symlink resolution.
use anyhow::{Context, Result};
use cap_std::fs::{Dir, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Component, Path};

#[derive(Clone, Copy)]
pub enum WritePolicy {
    Replace,
    Append,
    CreateOnly,
}

pub struct WorkspaceFiles {
    dir: Dir,
}

impl WorkspaceFiles {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            dir: Dir::open_ambient_dir(root, cap_std::ambient_authority())
                .with_context(|| format!("cannot open workspace directory {}", root.display()))?,
        })
    }

    fn validate(path: &Path) -> Result<()> {
        anyhow::ensure!(
            !path.as_os_str().is_empty() && !path.is_absolute(),
            "file path must be relative to the workspace"
        );
        anyhow::ensure!(
            !path.components().any(|c| matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )),
            "file path must not escape the workspace"
        );
        Ok(())
    }

    pub fn read(&self, path: impl AsRef<Path>) -> Result<Option<String>> {
        self.read_bytes(path)?
            .map(|bytes| String::from_utf8(bytes).context("file is not UTF-8"))
            .transpose()
    }

    pub fn read_bytes(&self, path: impl AsRef<Path>) -> Result<Option<Vec<u8>>> {
        let path = path.as_ref();
        Self::validate(path)?;
        match self.dir.read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(e).with_context(|| format!("cannot read workspace file {}", path.display()))
            }
        }
    }

    /// Atomic replacement never follows a final-component symlink or modifies
    /// another hard link to the original inode. CreateOnly uses an atomic link.
    pub fn write(
        &self,
        path: impl AsRef<Path>,
        content: &[u8],
        policy: WritePolicy,
    ) -> Result<bool> {
        let path = path.as_ref();
        Self::validate(path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        self.dir.create_dir_all(parent)?;
        let dir = self.dir.open_dir(parent)?;
        let name = path.file_name().context("file path has no filename")?;
        let metadata = match dir.symlink_metadata(name) {
            Ok(meta) => {
                anyhow::ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "refusing to write a symlink or non-regular file"
                );
                Some(meta)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if matches!(policy, WritePolicy::CreateOnly) && metadata.is_some() {
            return Ok(false);
        }
        let temp = format!(".devflow-{}.tmp", uuid::Uuid::new_v4());
        let result = (|| -> Result<bool> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = dir.open_with(&temp, &options)?;
            if let Some(meta) = &metadata {
                file.set_permissions(meta.permissions())?;
            }
            if matches!(policy, WritePolicy::Append) && metadata.is_some() {
                let mut source = dir.open(name)?;
                std::io::copy(&mut source, &mut file)?;
            }
            file.write_all(content)?;
            file.sync_all()?;
            if matches!(policy, WritePolicy::CreateOnly) {
                match dir.hard_link(&temp, &dir, name) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
                    Err(e) => return Err(e.into()),
                }
            } else {
                dir.rename(&temp, &dir, name)?;
            }
            Ok(true)
        })();
        let _ = dir.remove_file(&temp);
        result.with_context(|| format!("cannot write confined workspace file {}", path.display()))
    }

    pub fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<bool> {
        let content = self
            .read_bytes(from)?
            .context("copy source does not exist")?;
        let changed = self.write(
            to,
            &content,
            if overwrite {
                WritePolicy::Replace
            } else {
                WritePolicy::CreateOnly
            },
        )?;
        if changed {
            self.dir
                .set_permissions(to, self.dir.metadata(from)?.permissions())?;
        }
        Ok(changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn symlink_parents_and_targets_cannot_modify_external_files() {
        let root = std::env::temp_dir().join(format!("devflow-files-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("marker"), "original").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("marker"), workspace.join("target")).unwrap();
        let files = WorkspaceFiles::open(&workspace).unwrap();
        for path in ["escape/marker", "target", "../outside/marker"] {
            assert!(files.write(path, b"changed", WritePolicy::Replace).is_err());
        }
        assert_eq!(
            std::fs::read_to_string(outside.join("marker")).unwrap(),
            "original"
        );
        assert!(files
            .write("generated/.env", b"A=1\n", WritePolicy::CreateOnly)
            .unwrap());
        assert!(!files
            .write("generated/.env", b"A=2\n", WritePolicy::CreateOnly)
            .unwrap());
        assert_eq!(files.read("generated/.env").unwrap().unwrap(), "A=1\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
