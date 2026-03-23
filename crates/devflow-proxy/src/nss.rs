//! Firefox certificate trust via enterprise policies.
//!
//! On macOS, Firefox 72+ reads trusted CAs from the macOS Keychain
//! automatically (`security.enterprise_roots.enabled` defaults to `true`),
//! so no extra work is needed beyond `install_system_trust()`.
//!
//! On Linux, `ImportEnterpriseRoots` is NOT supported (Mozilla bug 1600509).
//! Instead, we write a `policies.json` file that tells Firefox to trust
//! the devflow CA certificate via `Certificates.Install`.

use anyhow::Result;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use crate::ca::default_ca_cert_path;

/// The standard system-wide Firefox policy path on Linux.
/// Works for deb/rpm installs. Snap/Flatpak may not honour this.
#[cfg(target_os = "linux")]
const LINUX_POLICY_DIR: &str = "/etc/firefox/policies";
#[cfg(target_os = "linux")]
const LINUX_POLICY_FILE: &str = "/etc/firefox/policies/policies.json";

/// Install the devflow CA into Firefox via `policies.json`.
///
/// On macOS this is a no-op — Firefox trusts the Keychain natively.
/// On Linux this writes `/etc/firefox/policies/policies.json` (requires root).
#[cfg(target_os = "macos")]
pub fn install_firefox_policy() -> Result<()> {
    // macOS: Firefox trusts the Keychain via ImportEnterpriseRoots (default).
    // Nothing to do.
    log::info!("macOS: Firefox trusts the system keychain natively");
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn install_firefox_policy() -> Result<()> {
    let cert_path = default_ca_cert_path();
    if !cert_path.exists() {
        anyhow::bail!("CA certificate not found at {}", cert_path.display());
    }
    let cert_path_str = cert_path.display().to_string();

    // Read existing policies.json if present, or start fresh
    let mut policies = if std::path::Path::new(LINUX_POLICY_FILE).exists() {
        let content = std::fs::read_to_string(LINUX_POLICY_FILE)
            .unwrap_or_else(|_| r#"{"policies":{}}"#.to_string());
        serde_json::from_str::<serde_json::Value>(&content)
            .unwrap_or_else(|_| serde_json::json!({"policies": {}}))
    } else {
        serde_json::json!({"policies": {}})
    };

    // Ensure policies.Certificates.Install array exists and contains our cert
    let certs = policies
        .pointer_mut("/policies/Certificates")
        .and_then(|c| c.as_object_mut());

    if let Some(certs_obj) = certs {
        if let Some(install_arr) = certs_obj.get_mut("Install").and_then(|v| v.as_array_mut()) {
            let already = install_arr
                .iter()
                .any(|v| v.as_str() == Some(&cert_path_str));
            if !already {
                install_arr.push(serde_json::json!(cert_path_str));
            }
        } else {
            certs_obj.insert("Install".to_string(), serde_json::json!([cert_path_str]));
        }
    } else {
        policies["policies"]["Certificates"] = serde_json::json!({
            "Install": [cert_path_str]
        });
    }

    let json = serde_json::to_string_pretty(&policies)?;
    write_policy_file(&json)?;

    log::info!("Firefox policy installed at {}", LINUX_POLICY_FILE);
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install_firefox_policy() -> Result<()> {
    Ok(())
}

/// Remove the devflow CA from Firefox's `policies.json`.
#[cfg(target_os = "macos")]
pub fn remove_firefox_policy() -> Result<()> {
    // macOS: nothing to clean up.
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn remove_firefox_policy() -> Result<()> {
    if !std::path::Path::new(LINUX_POLICY_FILE).exists() {
        return Ok(());
    }

    let cert_path_str = default_ca_cert_path().display().to_string();

    let content = std::fs::read_to_string(LINUX_POLICY_FILE)?;
    let mut policies: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"policies": {}}));

    if let Some(install_arr) = policies
        .pointer_mut("/policies/Certificates/Install")
        .and_then(|v| v.as_array_mut())
    {
        install_arr.retain(|v| v.as_str() != Some(&cert_path_str));

        // If Install array is now empty, remove the Certificates block
        if install_arr.is_empty() {
            if let Some(certs) = policies
                .pointer_mut("/policies/Certificates")
                .and_then(|v| v.as_object_mut())
            {
                certs.remove("Install");
                // If Certificates has no keys left, remove it too
                if certs.is_empty() {
                    if let Some(p) = policies
                        .pointer_mut("/policies")
                        .and_then(|v| v.as_object_mut())
                    {
                        p.remove("Certificates");
                    }
                }
            }
        }
    }

    let json = serde_json::to_string_pretty(&policies)?;
    write_policy_file(&json)?;

    log::info!("devflow CA removed from Firefox policy");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn remove_firefox_policy() -> Result<()> {
    Ok(())
}

/// Check if the devflow CA is trusted via Firefox policy.
///
/// On macOS: returns `true` — Firefox trusts the Keychain natively.
/// On Linux: checks if `policies.json` contains our cert path.
#[cfg(target_os = "macos")]
pub fn verify_firefox_policy() -> bool {
    // macOS: Firefox reads from the Keychain. If the CA is in the Keychain
    // (checked by verify_system_trust), Firefox trusts it automatically.
    true
}

#[cfg(target_os = "linux")]
pub fn verify_firefox_policy() -> bool {
    let cert_path_str = default_ca_cert_path().display().to_string();

    let content = match std::fs::read_to_string(LINUX_POLICY_FILE) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let policies: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    policies
        .pointer("/policies/Certificates/Install")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|v| v.as_str() == Some(&cert_path_str)))
        .unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn verify_firefox_policy() -> bool {
    false
}

/// Firefox policy paths for informational display.
pub fn firefox_policy_info() -> String {
    #[cfg(target_os = "macos")]
    {
        "Firefox trusts the macOS Keychain automatically (security.enterprise_roots.enabled)."
            .to_string()
    }

    #[cfg(target_os = "linux")]
    {
        format!(
            "Firefox policy file: {}\n\
            If Firefox was installed via Snap or Flatpak, the policy may not be detected.\n\
            You can import the CA manually in Firefox:\n\
            Settings > Privacy & Security > Certificates > View Certificates > Authorities > Import",
            LINUX_POLICY_FILE
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "Import the CA certificate manually in Firefox:\n\
        Settings > Privacy & Security > Certificates > View Certificates > Authorities > Import"
            .to_string()
    }
}

/// Return the Firefox policy file path, if applicable.
pub fn firefox_policy_path() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from(LINUX_POLICY_FILE))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

// --- private helpers ---

/// Write the policy JSON to the Firefox policies directory.
/// Requires root on Linux, so we use the same privilege escalation as
/// the system trust store installation.
#[cfg(target_os = "linux")]
fn write_policy_file(json: &str) -> Result<()> {
    let policy_dir = std::path::Path::new(LINUX_POLICY_DIR);
    if !policy_dir.exists() {
        run_privileged(
            "mkdir",
            &["-p", LINUX_POLICY_DIR],
            "create Firefox policy directory",
        )?;
    }

    // Write to a temp file, then move with privileges
    let tmp_path = "/tmp/devflow-firefox-policy.json";
    std::fs::write(tmp_path, json)?;

    run_privileged("cp", &[tmp_path, LINUX_POLICY_FILE], "write Firefox policy")?;
    run_privileged(
        "chmod",
        &["644", LINUX_POLICY_FILE],
        "set policy permissions",
    )?;

    // Clean up temp file
    let _ = std::fs::remove_file(tmp_path);

    Ok(())
}

/// Run a privileged command (sudo if TTY, pkexec otherwise).
/// Same approach as `platform.rs`.
#[cfg(target_os = "linux")]
fn run_privileged(program: &str, args: &[&str], action_desc: &str) -> Result<()> {
    use std::io::IsTerminal;
    use std::process::Command;

    let full_cmd = format!("{} {}", program, args.join(" "));

    if std::io::stdin().is_terminal() {
        let output = Command::new("sudo")
            .arg(program)
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run: sudo {}: {}", full_cmd, e))?;

        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sudo {} failed: {}", full_cmd, stderr.trim());
    }

    if which_exists("pkexec") {
        let output = Command::new("pkexec")
            .arg(program)
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to run: pkexec {}: {}", full_cmd, e))?;

        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "pkexec {} failed: {}\n\nYou can run the command manually:\n  sudo {}",
            full_cmd,
            stderr.trim(),
            full_cmd
        );
    }

    anyhow::bail!(
        "Cannot {} without a terminal (no TTY for sudo) and pkexec is not installed.\n\n\
        Please run manually:\n  sudo {}",
        action_desc,
        full_cmd
    )
}

#[cfg(target_os = "linux")]
fn which_exists(program: &str) -> bool {
    use std::process::Command;
    Command::new("which")
        .arg(program)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
