use crate::ca::CertificateAuthority;
use anyhow::Result;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::sync::{Arc, RwLock};

/// SNI-based certificate resolver that dynamically signs certificates.
pub struct SnsCertResolver {
    ca: Arc<CertificateAuthority>,
    /// Cache of parsed CertifiedKeys (rustls format), using std::sync::RwLock
    /// because ResolvesServerCert::resolve() is synchronous.
    key_cache: RwLock<std::collections::HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for SnsCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnsCertResolver").finish()
    }
}

impl SnsCertResolver {
    pub fn new(ca: Arc<CertificateAuthority>) -> Self {
        Self {
            ca,
            key_cache: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Build a CertifiedKey from PEM strings.
    fn build_certified_key(cert_pem: &str, key_pem: &str) -> Result<Arc<CertifiedKey>> {
        let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .filter_map(|c| c.ok())
            .collect();

        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("No private key found in PEM"))?;

        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)?;

        Ok(Arc::new(CertifiedKey::new(certs, signing_key)))
    }

    /// Pre-build a CertifiedKey for the given hostname.
    pub fn ensure_cert(&self, hostname: &str) -> Result<Arc<CertifiedKey>> {
        // Check cache first
        {
            let cache = self.key_cache.read().unwrap();
            if let Some(key) = cache.get(hostname) {
                return Ok(key.clone());
            }
        }

        // Generate via CA (synchronous — sign_server_cert is CPU-bound)
        let signed = self.ca.sign_server_cert(hostname)?;
        let certified = Self::build_certified_key(&signed.cert_pem, &signed.key_pem)?;

        // Cache it
        {
            let mut cache = self.key_cache.write().unwrap();
            cache.insert(hostname.to_string(), certified.clone());
        }

        Ok(certified)
    }
}

impl ResolvesServerCert for SnsCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let hostname = client_hello.server_name()?;

        // Check cache
        {
            let cache = self.key_cache.read().ok()?;
            if let Some(key) = cache.get(hostname) {
                return Some(key.clone());
            }
        }

        // Generate on the fly (synchronous)
        let signed = self.ca.sign_server_cert(hostname).ok()?;
        let certified = Self::build_certified_key(&signed.cert_pem, &signed.key_pem).ok()?;

        // Cache for future lookups
        if let Ok(mut cache) = self.key_cache.write() {
            cache.insert(hostname.to_string(), certified.clone());
        }

        Some(certified)
    }
}
