use anyhow::{bail, Result};
use regex::Regex;

/// Inspects shell command strings and blocks dangerous operations
/// in sandboxed workspaces.
pub struct CommandGuard {
    blocked_patterns: Vec<Regex>,
}

/// Default commands/patterns that are blocked in sandboxed mode.
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[
    // VCS push operations
    r"\bgit\s+push\b",
    r"\bjj\s+git\s+push\b",
    // GitHub/GitLab CLI destructive operations
    r"\bgh\s+pr\s+merge\b",
    r"\bgh\s+pr\s+close\b",
    r"\bgh\s+issue\s+close\b",
    r"\bglab\s+mr\s+merge\b",
    r"\bglab\s+mr\s+close\b",
    // Destructive filesystem operations
    r"\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|--recursive)\s+(/|~)",
    r"\bsudo\b",
    r"\bsu\s+-\b",
    // Package publish operations
    r"\bnpm\s+publish\b",
    r"\bcargo\s+publish\b",
    r"\bpip\s+upload\b",
    r"\bgem\s+push\b",
    r"\btwine\s+upload\b",
    // Remote access
    r"\bssh\s",
    r"\bscp\s",
];

impl CommandGuard {
    /// Create a guard with default blocked patterns.
    pub fn default_blocked() -> Self {
        let blocked_patterns = DEFAULT_BLOCKED_PATTERNS
            .iter()
            .filter_map(|p| Regex::new(p).ok())
            .collect();
        Self { blocked_patterns }
    }

    /// Create a guard from sandbox command config.
    ///
    /// - `extra_block`: additional patterns to block
    /// - `allow`: patterns to remove from the default block list
    pub fn from_config(extra_block: &[String], allow: &[String]) -> Self {
        let mut patterns: Vec<&str> = DEFAULT_BLOCKED_PATTERNS.to_vec();

        // Remove allowed patterns
        if !allow.is_empty() {
            patterns.retain(|p| {
                !allow.iter().any(|a| {
                    // Match by substring: if the allow entry appears in the pattern
                    p.contains(a.as_str())
                })
            });
        }

        let mut blocked_patterns: Vec<Regex> =
            patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();

        // Add extra block patterns
        for extra in extra_block {
            if let Ok(re) = Regex::new(extra) {
                blocked_patterns.push(re);
            }
        }

        Self { blocked_patterns }
    }

    /// Check if a command is allowed. Returns `Err` with a clear message if blocked.
    pub fn check(&self, command: &str) -> Result<()> {
        for pattern in &self.blocked_patterns {
            if pattern.is_match(command) {
                bail!(
                    "Command blocked by sandbox: '{}' matches restricted pattern '{}'",
                    command,
                    pattern.as_str()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blocks_git_push() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("git push origin main").is_err());
        assert!(guard.check("git push --force").is_err());
        assert!(guard.check("git push").is_err());
    }

    #[test]
    fn test_allows_git_commit() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("git commit -m 'test'").is_ok());
        assert!(guard.check("git add .").is_ok());
        assert!(guard.check("git status").is_ok());
        assert!(guard.check("git diff").is_ok());
    }

    #[test]
    fn test_blocks_npm_publish() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("npm publish").is_err());
        assert!(guard.check("npm publish --tag next").is_err());
    }

    #[test]
    fn test_allows_npm_install() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("npm install").is_ok());
        assert!(guard.check("npm run build").is_ok());
    }

    #[test]
    fn test_blocks_sudo() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("sudo rm -rf /").is_err());
        assert!(guard.check("sudo apt install foo").is_err());
    }

    #[test]
    fn test_blocks_ssh() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("ssh user@host").is_err());
        assert!(guard.check("scp file user@host:/tmp/").is_err());
    }

    #[test]
    fn test_blocks_gh_pr_merge() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("gh pr merge 123").is_err());
        assert!(guard.check("gh pr close 123").is_err());
        assert!(guard.check("gh issue close 42").is_err());
    }

    #[test]
    fn test_allows_gh_pr_create() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("gh pr create --title 'test'").is_ok());
        assert!(guard.check("gh pr list").is_ok());
    }

    #[test]
    fn test_blocks_destructive_rm() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("rm -rf /").is_err());
        assert!(guard.check("rm -rf ~").is_err());
    }

    #[test]
    fn test_allows_safe_rm() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("rm -rf ./build").is_ok());
        assert!(guard.check("rm temp.txt").is_ok());
    }

    #[test]
    fn test_extra_block_patterns() {
        let guard = CommandGuard::from_config(&["\\bdocker\\s+push\\b".to_string()], &[]);
        assert!(guard.check("docker push myimage").is_err());
        // Default blocks still apply
        assert!(guard.check("git push").is_err());
    }

    #[test]
    fn test_allow_overrides() {
        // Allow ssh and scp by removing them from blocked list
        let guard = CommandGuard::from_config(&[], &["ssh".to_string(), "scp".to_string()]);
        assert!(guard.check("ssh user@host").is_ok());
        assert!(guard.check("scp file user@host:/tmp/").is_ok());
        // Other blocks still apply
        assert!(guard.check("git push").is_err());
    }

    #[test]
    fn test_blocks_jj_push() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("jj git push").is_err());
    }

    #[test]
    fn test_blocks_cargo_publish() {
        let guard = CommandGuard::default_blocked();
        assert!(guard.check("cargo publish").is_err());
    }
}
