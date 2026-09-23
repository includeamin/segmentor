//! Optional native TLS: one certificate and key loaded once at startup, for a deployment with no
//! reverse proxy in front. See `docs/operations.md#tls`.
//!
//! This is deliberately small: no ACME, no renewal, no hot reload. A certificate that needs
//! automatic issuance or renewal (for example from Let's Encrypt) is what a reverse proxy or a
//! tool like Caddy already does well; segmentor only terminates the connection with whatever
//! `cert_path` and `key_path` currently hold, at the moment it starts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use tokio_rustls::rustls;

use crate::error::{Error, Result};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTls {
    cert_path: PathBuf,
    key_path: PathBuf,
    #[serde(default = "default_handshake_timeout_ms")]
    handshake_timeout_ms: u64,
}

const fn default_handshake_timeout_ms() -> u64 {
    10_000
}

/// A certificate and key, already parsed and validated, ready to terminate connections.
#[derive(Clone)]
pub(crate) struct TlsConfig {
    pub(crate) server_config: Arc<rustls::ServerConfig>,
    /// How long a client has to complete the handshake before the connection is dropped — the
    /// TLS-layer equivalent of `limits.header_read_timeout_ms`, and for the same reason: without
    /// it, a client that starts a handshake and goes quiet holds a connection slot forever.
    pub(crate) handshake_timeout_ms: u64,
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The certificate chain and key material are not printed; there is nothing about them
        // worth logging, and the key must never appear in a log line.
        formatter
            .debug_struct("TlsConfig")
            .field("handshake_timeout_ms", &self.handshake_timeout_ms)
            .finish_non_exhaustive()
    }
}

impl RawTls {
    pub(super) fn validate(&self, config_directory: &Path) -> Result<TlsConfig> {
        if self.handshake_timeout_ms == 0 {
            return Err(Error::Configuration(
                "server.tls.handshake_timeout_ms must be greater than zero".to_owned(),
            ));
        }
        let cert_path = resolve(&self.cert_path, config_directory);
        let key_path = resolve(&self.key_path, config_directory);

        let certs = CertificateDer::pem_file_iter(&cert_path)
            .and_then(Iterator::collect::<std::result::Result<Vec<_>, _>>)
            .map_err(|error| {
                Error::Configuration(format!(
                    "server.tls.cert_path {}: {error}",
                    cert_path.display()
                ))
            })?;
        if certs.is_empty() {
            return Err(Error::Configuration(format!(
                "server.tls.cert_path {} contains no certificates",
                cert_path.display()
            )));
        }
        let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|error| {
            Error::Configuration(format!(
                "server.tls.key_path {}: {error}",
                key_path.display()
            ))
        })?;

        // Installing a default crypto provider is process-wide and idempotent to call twice;
        // reqwest's own rustls client may already have installed one, and either way it is the
        // same backend (aws-lc-rs), so there is only ever one in the process.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|error| {
                Error::Configuration(format!(
                    "server.tls: certificate and key do not match: {error}"
                ))
            })?;
        // The server speaks HTTP/1.1 only today (see server.rs); advertising h2 over ALPN would
        // promise a protocol it does not actually serve.
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

        Ok(TlsConfig {
            server_config: Arc::new(server_config),
            handshake_timeout_ms: self.handshake_timeout_ms,
        })
    }
}

#[cfg(test)]
/// Loads a certificate and key directly, for other modules' tests that need a real `TlsConfig`
/// (`config::tls`'s own tests exercise `validate`'s error paths and belong here instead).
pub(crate) fn for_test(cert_path: PathBuf, key_path: PathBuf) -> Result<TlsConfig> {
    RawTls {
        cert_path,
        key_path,
        handshake_timeout_ms: default_handshake_timeout_ms(),
    }
    .validate(Path::new(""))
}

fn resolve(path: &Path, config_directory: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_directory.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_directory() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls")
    }

    #[test]
    fn loads_a_matching_certificate_and_key() {
        let raw = RawTls {
            cert_path: "cert.pem".into(),
            key_path: "key.pem".into(),
            handshake_timeout_ms: 5000,
        };

        let tls = raw
            .validate(&fixture_directory())
            .expect("a matching cert and key should load");

        assert_eq!(tls.handshake_timeout_ms, 5000);
        assert_eq!(tls.server_config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        assert!(
            !format!("{tls:?}").contains("BEGIN"),
            "no key material in Debug output"
        );
    }

    #[test]
    fn rejects_a_missing_file() {
        let raw = RawTls {
            cert_path: "does-not-exist.pem".into(),
            key_path: "key.pem".into(),
            handshake_timeout_ms: 5000,
        };

        let error = raw
            .validate(&fixture_directory())
            .expect_err("a missing file should fail");
        assert!(error.to_string().contains("server.tls.cert_path"));
    }

    #[test]
    fn rejects_a_key_that_does_not_match_the_certificate() {
        let raw = RawTls {
            cert_path: "cert.pem".into(),
            key_path: "mismatched-key.pem".into(),
            handshake_timeout_ms: 5000,
        };

        let error = raw
            .validate(&fixture_directory())
            .expect_err("a mismatched key should fail");
        assert!(error.to_string().contains("do not match"), "{error}");
    }

    #[test]
    fn rejects_a_zero_handshake_timeout() {
        let raw = RawTls {
            cert_path: "cert.pem".into(),
            key_path: "key.pem".into(),
            handshake_timeout_ms: 0,
        };

        assert!(raw.validate(&fixture_directory()).is_err());
    }
}
