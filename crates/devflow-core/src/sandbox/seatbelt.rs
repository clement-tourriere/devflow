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
    _extra_read_paths: &[PathBuf],
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

    // TTY job control (tcsetpgrp used by zsh/oh-my-zsh)
    profile.push_str("(allow file-ioctl (subpath \"/dev\"))\n");

    // Global read access — the sandbox restricts *writes*, not reads.
    // Users expect a normal shell: `ls /`, `cat /etc/hosts`, tool lookups, etc.
    profile.push_str("(allow file-read*)\n");

    // Workspace (write access)
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{}\"))\n",
        workspace
    ));

    // Temp directory (write access)
    profile.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", tmpdir));

    // Standard sink used heavily by shell startup scripts.
    profile.push_str("(allow file-write* (literal \"/dev/null\"))\n");

    // Extra write paths from config
    for path in extra_write_paths {
        profile.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            path.display()
        ));
    }

    // Real home cache/state dirs (oh-my-zsh, compinit resolve real home despite HOME override)
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{}/.cache\"))\n",
        home
    ));
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{}/.local/state\"))\n",
        home
    ));

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
pub fn wrap_command(command: &str, profile_path: &Path) -> (String, Vec<String>) {
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
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp/workspace\"))"));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn test_profile_allows_global_read_and_dev_null() {
        let profile = generate_seatbelt_profile(Path::new("/tmp/ws"), &[], &[]);

        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow file-write* (literal \"/dev/null\"))"));
    }

    #[test]
    fn test_extra_write_paths() {
        let profile =
            generate_seatbelt_profile(Path::new("/tmp/ws"), &[], &[PathBuf::from("/data/shared")]);

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

    #[cfg(target_os = "macos")]
    #[test]
    fn test_generated_profile_runs_basic_binary() {
        let sandbox_exec = Path::new("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            return;
        }

        let profile = generate_seatbelt_profile(Path::new("/tmp/devflow-sandbox-test"), &[], &[]);
        let profile_file =
            write_profile_to_temp(&profile).expect("failed to write seatbelt profile");

        let status = std::process::Command::new(sandbox_exec)
            .arg("-f")
            .arg(profile_file.path())
            .arg("/usr/bin/true")
            .status()
            .expect("failed to execute sandbox-exec");

        assert!(status.success(), "sandbox-exec returned status: {status}");
    }
}
