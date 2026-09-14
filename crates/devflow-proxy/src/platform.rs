//! Certificate diagnostics. Trust-store mutation belongs to system setup.
use crate::ca::{default_ca_cert_path, CertificateAuthority};
use anyhow::{Context, Result};

/// Legacy entry point: provide instructions instead of launching privileged tools.
pub fn install_system_trust(_ca: &CertificateAuthority) -> Result<()> {
    anyhow::bail!(
        "Automatic certificate installation was retired.\n{}",
        trust_info()
    )
}

pub fn remove_system_trust() -> Result<()> {
    anyhow::bail!("Automatic certificate removal was retired; remove the devflow Root CA using your system certificate manager.\n{}", trust_info())
}

/// Match the actual CA against native trusted roots, not a certificate name
/// or an unprocessed file in a trust-store input directory.
pub fn verify_system_trust() -> Result<bool> {
    let path = default_ca_cert_path();
    if !path.exists() {
        return Ok(false);
    }
    let pem = std::fs::read(&path)?;
    let cert = rustls_pemfile::certs(&mut pem.as_slice())
        .next()
        .context("CA file contains no certificate")??;
    let roots = rustls_native_certs::load_native_certs();
    if roots.certs.iter().any(|trusted| trusted == &cert) {
        return Ok(true);
    }
    anyhow::ensure!(
        roots.errors.is_empty(),
        "cannot inspect native certificate store: {:?}",
        roots.errors
    );
    Ok(false)
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
            security delete-certificate -c 'devflow Root CA'\n\n\
            Firefox: trusts macOS Keychain automatically",
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
            sudo cp {} /usr/local/share/ca-certificates/devflow.crt && sudo update-ca-certificates\n\n\
            {}",
            cert_path.display(),
            cert_path.display(),
            cert_path.display(),
            cert_path.display(),
            crate::nss::firefox_policy_info()
        )
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        format!(
            "CA certificate: {}\n\nPlease consult your system documentation for trust installation.\n\n\
            {}",
            cert_path.display(),
            crate::nss::firefox_policy_info()
        )
    }
}
