//! Install-time project detection for hook recipes.
//!
//! Recipes probe the project (files on disk, binaries actually installed)
//! when they are installed, so the generated hooks match the project's real
//! stack instead of guarding every possible tool behind runtime conditions.

use std::path::Path;

use crate::config::Config;

/// True if `bin` resolves on PATH or exists in the mise shims directory.
///
/// Hooks run via plain `sh -c` with the mise shims dir prepended to PATH
/// (see `actions/shell.rs`), so availability checks must see shim-managed
/// tools too.
pub fn binary_exists(bin: &str) -> bool {
    if which::which(bin).is_ok() {
        return true;
    }
    dirs::home_dir()
        .map(|home| home.join(".local/share/mise/shims").join(bin).is_file())
        .unwrap_or(false)
}

/// Project-probing context for recipe detection.
///
/// The binary checker is injectable so tests control tool availability
/// without depending on the host's PATH.
pub struct DetectContext<'a> {
    pub config: &'a Config,
    pub project_root: &'a Path,
    binary_checker: Box<dyn Fn(&str) -> bool + 'a>,
}

impl<'a> DetectContext<'a> {
    pub fn new(config: &'a Config, project_root: &'a Path) -> Self {
        Self::with_binary_checker(config, project_root, binary_exists)
    }

    pub fn with_binary_checker(
        config: &'a Config,
        project_root: &'a Path,
        checker: impl Fn(&str) -> bool + 'a,
    ) -> Self {
        Self {
            config,
            project_root,
            binary_checker: Box::new(checker),
        }
    }

    pub fn binary_exists(&self, bin: &str) -> bool {
        (self.binary_checker)(bin)
    }

    pub fn file_exists(&self, relative: &str) -> bool {
        self.project_root.join(relative).is_file()
    }

    pub fn dir_exists(&self, relative: &str) -> bool {
        self.project_root.join(relative).is_dir()
    }

    /// First candidate (in declaration order) that exists as a file.
    pub fn first_existing_file(&self, candidates: &[&str]) -> Option<String> {
        candidates
            .iter()
            .find(|c| self.file_exists(c))
            .map(|c| c.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binary_exists_finds_sh() {
        assert!(binary_exists("sh"));
        assert!(!binary_exists("definitely-missing-bin-devflow-xyz"));
    }

    #[test]
    fn test_injected_binary_checker() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = DetectContext::with_binary_checker(&config, tmp.path(), |bin| bin == "bun");
        assert!(ctx.binary_exists("bun"));
        assert!(!ctx.binary_exists("npm"));
    }

    #[test]
    fn test_file_probes_and_first_existing() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".env"), "").unwrap();
        std::fs::create_dir(tmp.path().join("migrations")).unwrap();

        let ctx = DetectContext::new(&config, tmp.path());
        assert!(ctx.file_exists(".env"));
        assert!(!ctx.file_exists(".env.local"));
        assert!(ctx.dir_exists("migrations"));
        assert!(!ctx.dir_exists("db"));
        // Order matters: first match wins even if later candidates exist too
        assert_eq!(
            ctx.first_existing_file(&[".env.local", ".env", ".env.development"]),
            Some(".env".to_string())
        );
        assert_eq!(ctx.first_existing_file(&["missing-a", "missing-b"]), None);
    }
}
