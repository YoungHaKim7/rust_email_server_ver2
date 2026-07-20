use anyhow::{Context, Result};
use rustls::ServerConfig as RustlsServerConfig;
use rustls_pemfile::{certs, private_key};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor as TokioTlsAcceptor;
use tracing::{debug, info, warn};

/// TLS configuration and certificate management
pub struct TlsConfig {
    pub acceptor: TokioTlsAcceptor,
}

impl TlsConfig {
    /// Create a new TLS configuration from certificate and key files
    pub fn new(cert_path: &Path, key_path: &Path) -> Result<Self> {
        info!("🔐 Loading TLS configuration");
        info!("  Certificate: {}", cert_path.display());
        info!("  Private Key: {}", key_path.display());

        // Load certificate chain
        let cert_file = File::open(cert_path)
            .with_context(|| format!("Failed to open certificate file: {}", cert_path.display()))?;
        let mut cert_reader = BufReader::new(cert_file);

        let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> = certs(&mut cert_reader)
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to read certificate chain")?;

        if cert_chain.is_empty() {
            return Err(anyhow::anyhow!("No certificates found in certificate file"));
        }

        debug!("✅ Loaded {} certificate(s)", cert_chain.len());

        // Load private key
        let key_file = File::open(key_path)
            .with_context(|| format!("Failed to open private key file: {}", key_path.display()))?;
        let mut key_reader = BufReader::new(key_file);

        let _private_key = private_key(&mut key_reader)
            .context("Failed to read private key")?
            .ok_or_else(|| anyhow::anyhow!("No private key found in key file"))?;

        debug!("✅ Loaded private key");

        // Convert certificate chain to owned 'static data
        let cert_chain_owned: Vec<rustls::pki_types::CertificateDer<'static>> = cert_chain
            .into_iter()
            .map(|cert| cert.to_owned())
            .collect();

        // Read the entire key file to get owned bytes
        let key_file = File::open(key_path)
            .with_context(|| format!("Failed to open private key file: {}", key_path.display()))?;

        let mut key_reader = BufReader::new(key_file);
        let mut key_bytes = Vec::new();
        std::io::copy(&mut key_reader, &mut key_bytes)
            .context("Failed to read private key bytes")?;

        // Create a new reader from the owned bytes to parse the key
        let key_cursor = std::io::Cursor::new(key_bytes);
        let mut key_reader = BufReader::new(key_cursor);
        let private_key_owned = rustls_pemfile::private_key(&mut key_reader)
            .context("Failed to parse private key from owned bytes")?
            .ok_or_else(|| anyhow::anyhow!("No private key found in key file"))?;

        // Create TLS configuration
        let config = Self::create_config(cert_chain_owned, private_key_owned)?;

        // Create TLS acceptor
        let acceptor = TokioTlsAcceptor::from(Arc::new(config));

        info!("✅ TLS configuration created successfully");

        Ok(TlsConfig { acceptor })
    }

    /// Create rustls ServerConfig from certificate and key
    fn create_config(
        cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
        private_key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<RustlsServerConfig> {
        // Create server config with modern TLS defaults
        let config = RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)
            .context("Failed to create TLS configuration with certificate")?;

        Ok(config)
    }

    /// Get the TLS acceptor
    pub fn acceptor(&self) -> &TokioTlsAcceptor {
        &self.acceptor
    }
}

/// Generate test certificates for development
pub fn generate_test_certificates() -> Result<()> {
    let cert_dir = "certs";
    let cert_path = format!("{}/server.crt", cert_dir);
    let key_path = format!("{}/server.key", cert_dir);

    // Create certs directory if it doesn't exist
    std::fs::create_dir_all(cert_dir)
        .context("Failed to create certificates directory")?;

    // Check if certificates already exist
    if Path::new(&cert_path).exists() && Path::new(&key_path).exists() {
        info!("✅ TLS certificates already exist:");
        info!("  Certificate: {}", cert_path);
        info!("  Private Key: {}", key_path);
        return Ok(());
    }

    info!("📋 Generating self-signed TLS certificates for development:");
    warn!("⚠️  These certificates are for TESTING ONLY - DO NOT use in production!");

    // Provide instructions for manual generation
    info!("📝 Please generate certificates manually using:");
    info!("$ mkdir -p {}", cert_dir);
    info!("$ openssl req -x509 -newkey rsa:2048 \\");
    info!("  -keyout {} \\", key_path);
    info!("  -out {} \\", cert_path);
    info!("  -days 365 \\");
    info!("  -nodes \\");
    info!("  -subj '/CN=localhost'");

    // Try to generate certificates automatically if openssl is available
    info!("🔧 Attempting to generate certificates automatically...");

    let result = std::process::Command::new("openssl")
        .arg("req")
        .arg("-x509")
        .arg("-newkey")
        .arg("rsa:2048")
        .arg("-keyout")
        .arg(&key_path)
        .arg("-out")
        .arg(&cert_path)
        .arg("-days")
        .arg("365")
        .arg("-nodes")
        .arg("-subj")
        .arg("/CN=localhost")
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                info!("✅ Successfully generated TLS certificates:");
                info!("  Certificate: {}", cert_path);
                info!("  Private Key: {}", key_path);
                Ok(())
            } else {
                let error_msg = String::from_utf8_lossy(&output.stderr);
                warn!("⚠️  Failed to generate certificates: {}", error_msg);
                Err(anyhow::anyhow!("OpenSSL certificate generation failed: {}", error_msg))
            }
        }
        Err(e) => {
            warn!("⚠️  OpenSSL not found: {}", e);
            Err(anyhow::anyhow!(
                "OpenSSL not found. Please install openssl or generate certificates manually using the commands above"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_certificates_instructions() {
        // This test just ensures the function runs without crashing
        // It will likely fail without openssl, which is expected
        let _ = generate_test_certificates();
    }
}