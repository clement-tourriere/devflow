//! The configuration files a proposal reads or would write, as they are now.
//!
//! Every file is read once, as text, and hashed. The hashes go into the
//! proposal so that `apply` can tell whether anything it depends on changed
//! between planning and applying.
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Project files devflow writes, in lookup order.
pub const DEVFLOW_FILES: [&str; 2] = ["devflow.toml", ".devflow.toml"];
/// mise files a proposal can write, in lookup order.
pub const MISE_FILES: [&str; 2] = ["mise.toml", ".mise.toml"];
/// The pitchfork file a proposal writes.
pub const PITCHFORK_FILE: &str = "pitchfork.toml";
/// The machine-local overlay, read for the merged configuration, never written.
pub const LOCAL_FILE: &str = "devflow.local.toml";

/// Every file a proposal reads or writes, relative to the project root. Files
/// that are only read still matter: a daemon defined in `pitchfork.local.toml`
/// or a `_.file` in `mise.local.toml` changes what is worth proposing.
const TRACKED: [&str; 11] = [
    "devflow.toml",
    ".devflow.toml",
    "devflow.local.toml",
    "pitchfork.toml",
    "pitchfork.local.toml",
    ".config/pitchfork.toml",
    "mise.toml",
    ".mise.toml",
    ".config/mise.toml",
    "mise.local.toml",
    ".mise.local.toml",
];

/// The key, in [`super::Proposal::input_hashes`], of the set of mise
/// configuration files other than the ones an apply writes (`mise.toml`,
/// `.mise.toml`): SHA-256 of their sorted paths. A file added or removed
/// anywhere mise reads changes it, `conf.d` and the environment-specific
/// files included, which the hashes of the files that exist cannot show for a
/// file that did not exist when the plan was made.
pub const MISE_LAYERS_KEY: &str = "mise-layers";

/// What a mise layer hashes to when it cannot be read.
const UNREADABLE: &str = "unreadable";

/// Files larger than this are refused: configuration is small, and a huge
/// file is not something to rewrite.
const MAX_BYTES: u64 = 1 << 20;

/// Text of the tracked files (`None` when a file does not exist), and the
/// hash of every other mise configuration file the detection layer finds.
///
/// mise layers its files (`.config/mise/conf.d/*.toml`, `config.local.toml`,
/// `mise.<env>.toml`, ...) and a later one can change what an earlier one
/// wired, so a plan depends on all of them, not just the two it can write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingFiles {
    texts: BTreeMap<String, Option<String>>,
    /// mise configuration files that are not in [`TRACKED`], by path.
    layers: BTreeMap<String, String>,
}

impl ExistingFiles {
    /// Read the tracked files under `root`. Symbolic links, files over 1 MiB
    /// and files that are not UTF-8 are errors: devflow will not read through
    /// a link or rewrite a file it cannot read whole.
    pub fn read(root: &Path) -> Result<Self> {
        let mut texts = BTreeMap::new();
        for name in TRACKED {
            texts.insert(name.to_string(), read_one(root, name)?);
        }
        // Every other file mise reads, found the way detection finds them. A
        // layer that cannot be read still counts: by what it hashes to.
        let layers = crate::detect::mise_layers(root)
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !TRACKED.contains(&path.as_str()))
            .map(|path| {
                let hash = layer_hash(&root.join(&path));
                (path, hash)
            })
            .collect();
        Ok(Self { texts, layers })
    }

    /// Existing files from in-memory text (tests, callers with their own
    /// source of truth). Every other tracked file is absent.
    pub fn from_texts<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut texts: BTreeMap<String, Option<String>> = TRACKED
            .iter()
            .map(|name| ((*name).to_string(), None))
            .collect();
        for (name, text) in files {
            texts.insert(name.to_string(), Some(text.to_string()));
        }
        Self {
            texts,
            layers: BTreeMap::new(),
        }
    }

    /// The text of `name`, when the file exists.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.texts.get(name).and_then(|text| text.as_deref())
    }

    /// The project file to edit: the one that exists, else `devflow.toml`.
    pub fn devflow_file(&self) -> &'static str {
        DEVFLOW_FILES
            .into_iter()
            .find(|name| self.text(name).is_some())
            .unwrap_or(DEVFLOW_FILES[0])
    }

    /// Whether a project file exists.
    pub fn has_devflow_file(&self) -> bool {
        DEVFLOW_FILES.iter().any(|name| self.text(name).is_some())
    }

    /// The mise file to edit: the one that exists, else `mise.toml`.
    pub fn mise_file(&self) -> &'static str {
        MISE_FILES
            .into_iter()
            .find(|name| self.text(name).is_some())
            .unwrap_or(MISE_FILES[0])
    }

    /// Whether `mise.toml` or `.mise.toml` exists.
    pub fn has_mise_file(&self) -> bool {
        MISE_FILES.iter().any(|name| self.text(name).is_some())
    }

    /// SHA-256 (hex) of every tracked file (`None` for one that is absent),
    /// of every other mise configuration file, and the marker of their set
    /// ([`MISE_LAYERS_KEY`]).
    pub fn hashes(&self) -> BTreeMap<String, Option<String>> {
        let mut hashes: BTreeMap<String, Option<String>> = self
            .texts
            .iter()
            .map(|(name, text)| (name.clone(), text.as_deref().map(sha256_hex)))
            .collect();
        for (path, hash) in &self.layers {
            hashes.insert(path.clone(), Some(hash.clone()));
        }
        hashes.insert(
            MISE_LAYERS_KEY.to_string(),
            Some(sha256_hex(&self.layer_paths().join("\n"))),
        );
        hashes
    }

    /// The paths of the mise configuration files that exist, sorted, without
    /// the ones an apply writes: those have hashes of their own, and
    /// the journaled apply changes them.
    fn layer_paths(&self) -> Vec<String> {
        let mut paths: BTreeSet<&str> = self.layers.keys().map(String::as_str).collect();
        for (name, text) in &self.texts {
            if text.is_some()
                && !MISE_FILES.contains(&name.as_str())
                && crate::detect::is_mise_layer(name)
            {
                paths.insert(name);
            }
        }
        paths.into_iter().map(str::to_string).collect()
    }
}

/// SHA-256 of a mise configuration file, as it is (links followed, like mise
/// does), or [`UNREADABLE`].
fn layer_hash(path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_BYTES => std::fs::read(path)
            .map(|bytes| sha256_bytes_hex(&bytes))
            .unwrap_or_else(|_| UNREADABLE.to_string()),
        _ => UNREADABLE.to_string(),
    }
}

/// SHA-256 of `text`, lowercase hex.
pub fn sha256_hex(text: &str) -> String {
    sha256_bytes_hex(text.as_bytes())
}

pub(crate) fn sha256_bytes_hex(bytes: &[u8]) -> String {
    crate::fsutil::sha256_hex(bytes)
}

/// The text of `name` under `root` as it is now (`None`: it does not exist),
/// under the same rules as [`ExistingFiles::read`].
pub(super) fn read_current(root: &Path, name: &str) -> Result<Option<String>> {
    read_one(root, name)
}

fn read_one(root: &Path, name: &str) -> Result<Option<String>> {
    let path = root.join(name);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // `.config/x` below a path component that is a file, or similar.
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "{name} is a symbolic link; devflow will not read through it or replace it. \
             Edit the file it points to by hand"
        );
    }
    if !metadata.is_file() {
        bail!("{name} is not a regular file");
    }
    if metadata.len() > MAX_BYTES {
        bail!("{name} is larger than 1 MiB; devflow will not rewrite it");
    }
    let bytes = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("{name} is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_none_and_hashes_cover_every_tracked_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[tools]\n").unwrap();
        let existing = ExistingFiles::read(dir.path()).unwrap();
        assert_eq!(existing.text("mise.toml"), Some("[tools]\n"));
        assert_eq!(existing.text("pitchfork.toml"), None);
        let hashes = existing.hashes();
        assert_eq!(
            hashes.len(),
            TRACKED.len() + 1,
            "the files and the layer marker"
        );
        assert_eq!(hashes["mise.toml"], Some(sha256_hex("[tools]\n")));
        assert_eq!(hashes["devflow.toml"], None);
        assert_eq!(existing.devflow_file(), "devflow.toml");
        assert_eq!(existing.mise_file(), "mise.toml");
        assert!(existing.has_mise_file());
        assert!(!existing.has_devflow_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_configuration_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.toml"), "x = 1\n").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.toml"), dir.path().join("mise.toml"))
            .unwrap();
        let error = ExistingFiles::read(dir.path()).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[test]
    fn a_dotfile_project_file_is_the_one_to_edit() {
        let existing = ExistingFiles::from_texts([(".devflow.toml", "x = 1\n")]);
        assert_eq!(existing.devflow_file(), ".devflow.toml");
    }
}
