use anyhow::{Context, Result};
use std::process::Command;

use crate::ca::{default_ca_cert_path, CertificateAuthority};

/// Detect Alpine Linux by checking for `/etc/alpine-release` or `/sbin/apk`.
#[cfg(target_os = "linux")]
fn is_alpine() -> bool {
    std::path::Path::new("/etc/alpine-release").exists()
        || std::path::Path::new("/sbin/apk").exists()
}

/// Check if a program exists on PATH.
#[cfg(target_os = "linux")]
fn which_exists(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a privileged command with appropriate escalation.
/// TTY → sudo, no TTY → pkexec, neither → error with manual instructions.
#[cfg(target_os = "linux")]
fn run_privileged(program: &str, args: &[&str], action_desc: &str) -> Result<()> {
    use std::io::IsTerminal;

    let full_cmd = format!("{} {}", program, args.join(" "));

    if std::io::stdin().is_terminal() {
        // TTY available — use sudo
        let output = Command::new("sudo")
            .arg(program)
            .args(args)
            .output()
            .with_context(|| format!("Failed to run: sudo {}", full_cmd))?;

        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sudo {} failed: {}", full_cmd, stderr.trim());
    }

    // No TTY — try pkexec for graphical auth
    if which_exists("pkexec") {
        let output = Command::new("pkexec")
            .arg(program)
            .args(args)
            .output()
            .with_context(|| format!("Failed to run: pkexec {}", full_cmd))?;

        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "pkexec {} failed: {}\n\nYou can run the command manually in a terminal:\n  sudo {}",
            full_cmd,
            stderr.trim(),
            full_cmd
        );
    }

    // No sudo TTY and no pkexec — provide manual instructions
    anyhow::bail!(
        "Cannot {} without a terminal (no TTY for sudo) and pkexec is not installed.\n\n\
        Please run the following command manually in a terminal:\n  sudo {}",
        action_desc,
        full_cmd
    )
}

/// Install the CA certificate in the system trust store.
#[cfg(target_os = "macos")]
pub fn install_system_trust(ca: &CertificateAuthority) -> Result<()> {
    let cert_path = default_ca_cert_path();

    if !cert_path.exists() {
        ca.save(&cert_path, &crate::ca::default_ca_key_path())?;
    }

    let cert_path_str = cert_path.to_str().unwrap();

    // macOS 12+ allows adding to login keychain without sudo
    let home = std::env::var("HOME").unwrap_or_default();
    let login_keychain = format!("{}/Library/Keychains/login.keychain-db", home);

    let output = Command::new("security")
        .args([
            "add-trusted-cert",
            "-r",
            "trustRoot",
            "-k",
            &login_keychain,
            cert_path_str,
        ])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            log::info!("CA certificate installed to login keychain");
            return Ok(());
        }
    }

    anyhow::bail!(
        "Could not install automatically. Please run:\n\n\
        sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain '{}'\n",
        cert_path_str
    )
}

/// Install the CA certificate in the system trust store.
#[cfg(target_os = "linux")]
pub fn install_system_trust(ca: &CertificateAuthority) -> Result<()> {
    let cert_path = default_ca_cert_path();

    if !cert_path.exists() {
        ca.save(&cert_path, &crate::ca::default_ca_key_path())?;
    }

    let cert_path_str = cert_path.to_str().unwrap();

    // Try Debian/Ubuntu path first
    let debian_path = "/usr/local/share/ca-certificates/devflow.crt";
    let rhel_path = "/etc/pki/ca-trust/source/anchors/devflow.crt";

    if std::path::Path::new("/usr/local/share/ca-certificates").exists() {
        run_privileged("cp", &[cert_path_str, debian_path], "copy certificate")?;
        run_privileged("update-ca-certificates", &[], "update system certificates")?;
        log::info!("CA certificate installed (Debian/Ubuntu)");
        return Ok(());
    } else if std::path::Path::new("/etc/pki/ca-trust/source/anchors").exists() {
        run_privileged("cp", &[cert_path_str, rhel_path], "copy certificate")?;
        run_privileged("update-ca-trust", &[], "update system trust")?;
        log::info!("CA certificate installed (RHEL/Fedora)");
        return Ok(());
    } else if is_alpine() {
        let alpine_path = "/usr/local/share/ca-certificates/devflow.crt";
        run_privileged(
            "mkdir",
            &["-p", "/usr/local/share/ca-certificates"],
            "create ca-certificates directory",
        )?;
        run_privileged("cp", &[cert_path_str, alpine_path], "copy certificate")?;
        run_privileged("update-ca-certificates", &[], "update system certificates")?;
        log::info!("CA certificate installed (Alpine Linux)");
        return Ok(());
    }

    anyhow::bail!(
        "Could not install automatically. Please copy {} to your system CA store and run update-ca-certificates or update-ca-trust.",
        cert_path.display()
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install_system_trust(_ca: &CertificateAuthority) -> Result<()> {
    anyhow::bail!("Automatic CA installation not implemented for this platform.")
}

/// Check if the CA certificate is trusted by the system.
#[cfg(target_os = "macos")]
pub fn verify_system_trust() -> Result<bool> {
    let cert_path = default_ca_cert_path();
    if !cert_path.exists() {
        return Ok(false);
    }

    // Check if the certificate is in any keychain
    let output = Command::new("security")
        .args(["find-certificate", "-c", "devflow Root CA"])
        .output()
        .context("Failed to run security command")?;

    Ok(output.status.success())
}

/// Check if the CA certificate is trusted by the system.
#[cfg(target_os = "linux")]
pub fn verify_system_trust() -> Result<bool> {
    let debian_path = std::path::Path::new("/usr/local/share/ca-certificates/devflow.crt");
    let rhel_path = std::path::Path::new("/etc/pki/ca-trust/source/anchors/devflow.crt");
    // Alpine uses the same path as Debian (/usr/local/share/ca-certificates/)
    Ok(debian_path.exists() || rhel_path.exists())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn verify_system_trust() -> Result<bool> {
    Ok(false)
}

/// Remove the CA certificate from the system trust store.
#[cfg(target_os = "macos")]
pub fn remove_system_trust() -> Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let login_keychain = format!("{}/Library/Keychains/login.keychain-db", home);

    // Remove from login keychain by CN
    let output = Command::new("security")
        .args([
            "delete-certificate",
            "-c",
            "devflow Root CA",
            "-t",
            &login_keychain,
        ])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            log::info!("CA certificate removed from login keychain");
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        log::warn!("security delete-certificate failed: {}", stderr);
    }

    // Fallback: try without specifying keychain (searches default)
    let output = Command::new("security")
        .args(["delete-certificate", "-c", "devflow Root CA"])
        .output();

    if let Ok(out) = output {
        if out.status.success() {
            log::info!("CA certificate removed from default keychain");
            return Ok(());
        }
    }

    anyhow::bail!(
        "Could not remove automatically. Please open Keychain Access and delete the 'devflow Root CA' certificate."
    )
}

#[cfg(target_os = "linux")]
pub fn remove_system_trust() -> Result<()> {
    let debian_path = "/usr/local/share/ca-certificates/devflow.crt";
    let rhel_path = "/etc/pki/ca-trust/source/anchors/devflow.crt";

    // Debian/Ubuntu and Alpine use the same path
    if std::path::Path::new(debian_path).exists() {
        run_privileged("rm", &[debian_path], "remove certificate")?;
        run_privileged(
            "update-ca-certificates",
            &["--fresh"],
            "update system certificates",
        )?;
        return Ok(());
    }

    if std::path::Path::new(rhel_path).exists() {
        run_privileged("rm", &[rhel_path], "remove certificate")?;
        run_privileged("update-ca-trust", &[], "update system trust")?;
        return Ok(());
    }

    anyhow::bail!("No system trust certificate found to remove.")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn remove_system_trust() -> Result<()> {
    anyhow::bail!("Automatic CA removal not implemented for this platform.")
}

/// Get trust status information.
pub fn trust_info() -> String {
    let cert_path = default_ca_cert_path();

    #[cfg(target_os = "macos")]
    {
        format!(
            "CA certificate: {}\n\n\
            To trust manually:\n\
            sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain '{}'\n\n\
            To remove:\n\
            security delete-certificate -c 'devflow Root CA'",
            cert_path.display(),
            cert_path.display()
        )
    }

    #[cfg(target_os = "linux")]
    {
        format!(
            "CA certificate: {}\n\n\
            Ubuntu/Debian:\n\
            sudo cp {} /usr/local/share/ca-certificates/devflow.crt && sudo update-ca-certificates\n\n\
            Fedora/RHEL:\n\
            sudo cp {} /etc/pki/ca-trust/source/anchors/devflow.crt && sudo update-ca-trust\n\n\
            Alpine Linux:\n\
            sudo cp {} /usr/local/share/ca-certificates/devflow.crt && sudo update-ca-certificates",
            cert_path.display(),
            cert_path.display(),
            cert_path.display(),
            cert_path.display()
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        format!(
            "CA certificate: {}\n\nPlease consult your system documentation for trust installation.",
            cert_path.display()
        )
    }
}
