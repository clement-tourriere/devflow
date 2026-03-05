use std::io::Write;
use std::path::{Path, PathBuf};

/// Generate a macOS Seatbelt (.sb) profile for sandboxed execution.
///
/// The profile allows:
/// - Read access to system binaries, libraries, and user tool directories
/// - Read+write access to the workspace directory and temp directory
/// - Full network access (v1 doesn't restrict network)
/// - Process execution and forking
pub fn generate_seatbelt_profile(
    workspace_dir: &Path,
    extra_read_paths: &[PathBuf],
    extra_write_paths: &[PathBuf],
) -> String {
    let home = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/Users/unknown".to_string());
    let tmpdir = std::env::var("TMPDIR")
        .unwrap_or_else(|_| "/tmp".to_string())
        .trim_end_matches('/')
        .to_string();
    let workspace = workspace_dir.display().to_string();

    let mut profile = String::new();
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n");
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow signal)\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow mach-register)\n");
    profile.push_str("(allow ipc-posix-shm-read-data)\n");
    profile.push_str("(allow ipc-posix-shm-write-data)\n");

    // System binaries + libraries (read-only)
    profile.push_str("(allow file-read*\n");
    profile.push_str("  (subpath \"/usr\")\n");
    profile.push_str("  (subpath \"/bin\")\n");
    profile.push_str("  (subpath \"/sbin\")\n");
    profile.push_str("  (subpath \"/Library\")\n");
    profile.push_str("  (subpath \"/System\")\n");
    profile.push_str("  (subpath \"/opt/homebrew\")\n");
    profile.push_str("  (subpath \"/nix\")\n");
    profile.push_str("  (subpath \"/private/var\")\n");
    profile.push_str("  (subpath \"/private/tmp\")\n");
    profile.push_str("  (subpath \"/dev\")\n");
    profile.push_str("  (subpath \"/etc\")\n");
    profile.push_str("  (subpath \"/var\")\n");
    profile.push_str("  (subpath \"/Applications\")\n");

    // User tool directories (read-only)
    profile.push_str(&format!("  (subpath \"{}/.cargo\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.rustup\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.nvm\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.local\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.bun\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.npm\")\n", home));
    profile.push_str(&format!("  (subpath \"{}/.config\")\n", home));

    // Extra read paths from config
    for path in extra_read_paths {
        profile.push_str(&format!("  (subpath \"{}\")\n", path.display()));
    }

    profile.push_str(")\n");

    // Workspace (read+write)
    profile.push_str(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))\n",
        workspace
    ));

    // Temp directory (read+write)
    profile.push_str(&format!(
        "(allow file-read* file-write* (subpath \"{}\"))\n",
        tmpdir
    ));

    // Extra write paths from config
    for path in extra_write_paths {
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{}\"))\n",
            path.display()
        ));
    }

    // Network — allow all (v1 doesn't restrict network)
    profile.push_str("(allow network*)\n");

    profile
}

/// Write a Seatbelt profile to a temporary file and return the path.
pub fn write_profile_to_temp(profile: &str) -> std::io::Result<tempfile::NamedTempFile> {
    let mut tmp = tempfile::Builder::new()
        .prefix("devflow-sandbox-")
        .suffix(".sb")
        .tempfile()?;
    tmp.write_all(profile.as_bytes())?;
    tmp.flush()?;
    Ok(tmp)
}

/// Wrap a command string to run under sandbox-exec.
/// Returns (program, args) tuple suitable for `Command::new(program).args(args)`.
pub fn wrap_command(
    command: &str,
    profile_path: &Path,
) -> (String, Vec<String>) {
    (
        "sandbox-exec".to_string(),
        vec![
            "-f".to_string(),
            profile_path.display().to_string(),
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_profile_generation() {
        let profile = generate_seatbelt_profile(
            Path::new("/tmp/workspace"),
            &[PathBuf::from("/opt/shared-data")],
            &[],
        );

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow process-exec)"));
        assert!(profile.contains("(subpath \"/tmp/workspace\")"));
        assert!(profile.contains("(subpath \"/opt/shared-data\")"));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn test_profile_includes_system_paths() {
        let profile = generate_seatbelt_profile(Path::new("/tmp/ws"), &[], &[]);

        assert!(profile.contains("(subpath \"/usr\")"));
        assert!(profile.contains("(subpath \"/bin\")"));
        assert!(profile.contains("(subpath \"/Library\")"));
        assert!(profile.contains("(subpath \"/opt/homebrew\")"));
    }

    #[test]
    fn test_extra_write_paths() {
        let profile = generate_seatbelt_profile(
            Path::new("/tmp/ws"),
            &[],
            &[PathBuf::from("/data/shared")],
        );

        assert!(profile.contains("(subpath \"/data/shared\")"));
    }

    #[test]
    fn test_wrap_command() {
        let (prog, args) = wrap_command("echo hello", Path::new("/tmp/profile.sb"));
        assert_eq!(prog, "sandbox-exec");
        assert_eq!(
            args,
            vec!["-f", "/tmp/profile.sb", "sh", "-c", "echo hello"]
        );
    }
}
