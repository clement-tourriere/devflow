use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tokio::sync::RwLock;

/// Default CA certificate path: ~/.devflow/proxy/ca.crt
pub fn default_ca_cert_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".devflow")
        .join("proxy")
        .join("ca.crt")
}

/// Default CA key path: ~/.devflow/proxy/ca.key
pub fn default_ca_key_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".devflow")
        .join("proxy")
        .join("ca.key")
}

/// Certificate Authority for signing server certificates.
pub struct CertificateAuthority {
    ca_cert: rcgen::Certificate,
    key_pair: KeyPair,
    ca_cert_pem: String,
    ca_key_pem: String,
}

impl CertificateAuthority {
    /// Generate a new Certificate Authority.
    pub fn generate() -> Result<Self> {
        log::info!("Generating new Certificate Authority");

        let mut params = CertificateParams::default();

        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "devflow Root CA");
        dn.push(DnType::OrganizationName, "devflow");
        params.distinguished_name = dn;

        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];

        params.not_before = OffsetDateTime::now_utc();
        params.not_after = params.not_before + Duration::days(365 * 10);

        let key_pair = KeyPair::generate().context("Failed to generate key pair")?;

        let ca_cert = params
            .self_signed(&key_pair)
            .context("Failed to generate self-signed CA certificate")?;

        let ca_cert_pem = ca_cert.pem();
        let ca_key_pem = key_pair.serialize_pem();

        Ok(Self {
            ca_cert,
            key_pair,
            ca_cert_pem,
            ca_key_pem,
        })
    }

    /// Load a Certificate Authority from PEM files.
    pub fn load(cert_path: &Path, key_path: &Path) -> Result<Self> {
        log::info!("Loading CA from {:?}", cert_path);

        let ca_cert_pem = fs::read_to_string(cert_path).context("Failed to read CA certificate")?;
        let ca_key_pem = fs::read_to_string(key_path).context("Failed to read CA key")?;

        let key_pair = KeyPair::from_pem(&ca_key_pem).context("Failed to parse CA private key")?;

        let params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)
            .context("Failed to parse CA certificate parameters")?;

        let ca_cert = params
            .self_signed(&key_pair)
            .context("Failed to recreate CA certificate")?;

        Ok(Self {
            ca_cert,
            key_pair,
            ca_cert_pem,
            ca_key_pem,
        })
    }

    /// Load CA from default paths, or generate if not found.
    pub fn load_or_generate() -> Result<Self> {
        let cert_path = default_ca_cert_path();
        let key_path = default_ca_key_path();

        if cert_path.exists() && key_path.exists() {
            Self::load(&cert_path, &key_path)
        } else {
            let ca = Self::generate()?;
            ca.save(&cert_path, &key_path)?;
            Ok(ca)
        }
    }

    /// Save the CA certificate and key to files.
    pub fn save(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        if let Some(parent) = cert_path.parent() {
            fs::create_dir_all(parent).context("Failed to create CA directory")?;
        }

        fs::write(cert_path, &self.ca_cert_pem).context("Failed to write CA certificate")?;
        fs::write(key_path, &self.ca_key_pem).context("Failed to write CA key")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(key_path)?.permissions();
            perms.set_mode(0o600);
            fs::set_permissions(key_path, perms)?;
        }

        log::info!("Saved CA to {:?}", cert_path);
        Ok(())
    }

    /// Get the PEM-encoded CA certificate.
    pub fn cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Sign a server certificate for the given hostname.
    pub fn sign_server_cert(&self, hostname: &str) -> Result<SignedCertificate> {
        log::debug!("Signing certificate for {}", hostname);

        let mut params = CertificateParams::default();

        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, hostname);
        params.distinguished_name = dn;

        params.subject_alt_names = vec![SanType::DnsName(hostname.try_into()?)];

        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];

        params.not_before = OffsetDateTime::now_utc();
        params.not_after = params.not_before + Duration::days(365);

        let server_key = KeyPair::generate().context("Failed to generate server key pair")?;

        let server_cert = params
            .signed_by(&server_key, &self.ca_cert, &self.key_pair)
            .context("Failed to sign server certificate")?;

        Ok(SignedCertificate {
            cert_pem: server_cert.pem(),
            key_pem: server_key.serialize_pem(),
        })
    }
}

/// A signed server certificate with its private key.
pub struct SignedCertificate {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Thread-safe certificate cache for signed server certificates.
pub struct CertificateCache {
    ca: Arc<CertificateAuthority>,
    cache: RwLock<HashMap<String, Arc<SignedCertificate>>>,
}

impl CertificateCache {
    /// Create a new certificate cache with the given CA.
    pub fn new(ca: Arc<CertificateAuthority>) -> Self {
        Self {
            ca,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Get or create a signed certificate for the given hostname.
    pub async fn get_cert(&self, hostname: &str) -> Result<Arc<SignedCertificate>> {
        {
            let cache = self.cache.read().await;
            if let Some(cert) = cache.get(hostname) {
                return Ok(cert.clone());
            }
        }

        let cert = self.ca.sign_server_cert(hostname)?;
        let cert = Arc::new(cert);

        {
            let mut cache = self.cache.write().await;
            cache.insert(hostname.to_string(), cert.clone());
        }

        Ok(cert)
    }

    /// Get the CA certificate PEM.
    pub fn ca_cert_pem(&self) -> &str {
        self.ca.cert_pem()
    }

    /// Get reference to the CA.
    pub fn ca(&self) -> &CertificateAuthority {
        &self.ca
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_ca_generation() {
        let ca = CertificateAuthority::generate().unwrap();
        assert!(ca.cert_pem().contains("BEGIN CERTIFICATE"));
    }

    #[test]
    fn test_ca_save_load() {
        let dir = tempdir().unwrap();
        let cert_path = dir.path().join("ca.crt");
        let key_path = dir.path().join("ca.key");

        let ca = CertificateAuthority::generate().unwrap();
        ca.save(&cert_path, &key_path).unwrap();

        let loaded = CertificateAuthority::load(&cert_path, &key_path);
        assert!(loaded.is_ok());
    }

    #[test]
    fn test_server_cert_signing() {
        let ca = CertificateAuthority::generate().unwrap();
        let cert = ca.sign_server_cert("example.localhost").unwrap();
        assert!(cert.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cert.key_pem.contains("BEGIN PRIVATE KEY"));
    }
}
