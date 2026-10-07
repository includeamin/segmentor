//! `[authorization]`: signed playback tokens (TDD 0007). Absent, nothing is checked and the origin
//! serves exactly as before.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::authorization::{Algorithm, Key, Verifier};
use crate::error::{Error, Result};

const MAX_CLOCK_SKEW_SECS: u64 = 300;
const MIN_TOKEN_BYTES: usize = 64;
const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// Where a token may arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent switches, one per transport"
)]
pub(crate) struct Transports {
    /// `Authorization: Bearer <token>`.
    pub(crate) header: bool,
    /// A cookie, which keeps the token out of the URL and so out of a CDN's cache key.
    pub(crate) cookie: bool,
    /// A query parameter, which the playlists carry into every URL they list, so a player that
    /// only follows those URLs needs no configuration.
    pub(crate) query: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawAuthorization {
    /// `HS256`, `ES256`, or `EdDSA`: the one algorithm tokens must use.
    algorithm: String,
    /// The environment variable holding the key: the shared secret for `HS256`, a PEM public key
    /// for the others. The value is never in this file.
    key_env: Option<String>,
    /// A file holding the key instead, for a PEM public key an operator would rather mount.
    key_file: Option<PathBuf>,
    #[serde(default = "default_transports")]
    transports: Vec<String>,
    #[serde(default = "default_query_parameter")]
    query_parameter: String,
    #[serde(default = "default_cookie_name")]
    cookie_name: String,
    #[serde(default = "default_clock_skew_secs")]
    clock_skew_secs: u64,
    #[serde(default = "default_max_token_bytes")]
    max_token_bytes: usize,
}

fn default_transports() -> Vec<String> {
    ["header", "cookie", "query"].map(str::to_owned).to_vec()
}

fn default_query_parameter() -> String {
    "auth".to_owned()
}

fn default_cookie_name() -> String {
    "segmentor_auth".to_owned()
}

const fn default_clock_skew_secs() -> u64 {
    30
}

const fn default_max_token_bytes() -> usize {
    4096
}

/// The validated settings.
#[derive(Debug, Clone)]
pub(crate) struct AuthorizationSettings {
    pub(crate) verifier: Verifier,
    pub(crate) transports: Transports,
    pub(crate) query_parameter: String,
    pub(crate) cookie_name: String,
}

impl RawAuthorization {
    pub(super) fn validate(
        self,
        config_directory: &Path,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<AuthorizationSettings> {
        let fail = |message: String| Err(Error::Configuration(message));
        let algorithm = match self.algorithm.as_str() {
            "HS256" => Algorithm::Hs256,
            "ES256" => Algorithm::Es256,
            "EdDSA" => Algorithm::EdDsa,
            other => {
                return fail(format!(
                    "authorization.algorithm `{other}` is not supported; use HS256, ES256, or EdDSA"
                ));
            }
        };
        if self.clock_skew_secs > MAX_CLOCK_SKEW_SECS {
            return fail(format!(
                "authorization.clock_skew_secs must be at most {MAX_CLOCK_SKEW_SECS}"
            ));
        }
        if !(MIN_TOKEN_BYTES..=MAX_TOKEN_BYTES).contains(&self.max_token_bytes) {
            return fail(format!(
                "authorization.max_token_bytes must be between {MIN_TOKEN_BYTES} and {MAX_TOKEN_BYTES}"
            ));
        }
        let mut transports = Transports {
            header: false,
            cookie: false,
            query: false,
        };
        for name in &self.transports {
            let slot = match name.as_str() {
                "header" => &mut transports.header,
                "cookie" => &mut transports.cookie,
                "query" => &mut transports.query,
                other => {
                    return fail(format!(
                        "authorization.transports `{other}` is not one of header, cookie, query"
                    ));
                }
            };
            if std::mem::replace(slot, true) {
                return fail(format!("authorization.transports lists `{name}` twice"));
            }
        }
        if !(transports.header || transports.cookie || transports.query) {
            return fail("authorization.transports must name at least one transport".to_owned());
        }
        let name_ok = |name: &str| {
            (1..=64).contains(&name.len())
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        };
        if !name_ok(&self.query_parameter) || !name_ok(&self.cookie_name) {
            return fail(
                "authorization.query_parameter and cookie_name are 1 to 64 letters, digits, `_`, or `-`"
                    .to_owned(),
            );
        }

        let material = match (self.key_env, self.key_file) {
            (Some(name), None) => environment(&name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::Configuration(format!(
                        "authorization.key_env names `{name}`, which is not set"
                    ))
                })?,
            (None, Some(path)) => {
                let path = if path.is_absolute() {
                    path
                } else {
                    config_directory.join(path)
                };
                std::fs::read_to_string(&path).map_err(|error| {
                    Error::Configuration(format!(
                        "authorization.key_file {} cannot be read: {error}",
                        path.display()
                    ))
                })?
            }
            _ => {
                return fail("authorization needs exactly one of key_env and key_file".to_owned());
            }
        };
        // A trailing newline is how a secret usually arrives from a file or a shell.
        let material = material.trim_end_matches(['\n', '\r']);
        let key = Key::parse(algorithm, material)
            .map_err(|reason| Error::Configuration(format!("authorization key: {reason}")))?;
        Ok(AuthorizationSettings {
            verifier: Verifier::new(
                algorithm,
                key,
                Duration::from_secs(self.clock_skew_secs),
                self.max_token_bytes,
            ),
            transports,
            query_parameter: self.query_parameter,
            cookie_name: self.cookie_name,
        })
    }
}
