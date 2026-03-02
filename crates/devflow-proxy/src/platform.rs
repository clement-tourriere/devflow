use anyhow::{Context, Result};
use std::process::Command;

use crate::ca::{default_ca_cert_path, CertificateAuthority};

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

    // Try Debian/Ubuntu path first
    let debian_path = "/usr/local/share/ca-certificates/devflow.crt";
    let rhel_path = "/etc/pki/ca-trust/source/anchors/devflow.crt";

    if std::path::Path::new("/usr/local/share/ca-certificates").exists() {
        let status = Command::new("sudo")
            .args(["cp", cert_path.to_str().unwrap(), debian_path])
            .status()
            .context("Failed to copy certificate")?;

        if status.success() {
            let status = Command::new("sudo")
                .args(["update-ca-certificates"])
                .status()
                .context("Failed to update certificates")?;

            if status.success() {
                log::info!("CA certificate installed (Debian/Ubuntu)");
                return Ok(());
            }
        }
    } else if std::path::Path::new("/etc/pki/ca-trust/source/anchors").exists() {
        let status = Command::new("sudo")
            .args(["cp", cert_path.to_str().unwrap(), rhel_path])
            .status()
            .context("Failed to copy certificate")?;

        if status.success() {
            let status = Command::new("sudo")
                .args(["update-ca-trust"])
                .status()
                .context("Failed to update trust")?;

            if status.success() {
                log::info!("CA certificate installed (RHEL/Fedora)");
                return Ok(());
            }
        }
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

    if std::path::Path::new(debian_path).exists() {
        Command::new("sudo")
            .args(["rm", debian_path])
            .status()
            .context("Failed to remove certificate")?;
        Command::new("sudo")
            .args(["update-ca-certificates", "--fresh"])
            .status()
            .context("Failed to update certificates")?;
        return Ok(());
    }

    if std::path::Path::new(rhel_path).exists() {
        Command::new("sudo")
            .args(["rm", rhel_path])
            .status()
            .context("Failed to remove certificate")?;
        Command::new("sudo")
            .args(["update-ca-trust"])
            .status()
            .context("Failed to update trust")?;
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
            sudo cp {} /etc/pki/ca-trust/source/anchors/devflow.crt && sudo update-ca-trust",
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
