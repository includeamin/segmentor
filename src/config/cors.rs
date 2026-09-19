//! CORS policy configuration.
//!
//! Values are plain strings validated for shape here; `http::cors` parses them into HTTP types.

use serde::Deserialize;

use crate::error::{Error, Result};

/// Cross-origin resource sharing policy for browser players.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct CorsConfig {
    /// Set to `false` when a fronting proxy or CDN adds CORS headers itself.
    pub(crate) enabled: bool,
    /// Exact origins such as `https://player.example.com`, or `["*"]` for any origin.
    pub(crate) allowed_origins: Vec<String>,
    pub(crate) allowed_methods: Vec<String>,
    /// Request headers browsers may send; `["*"]` allows any.
    pub(crate) allowed_headers: Vec<String>,
    /// Response headers scripts may read; `["*"]` exposes all.
    pub(crate) exposed_headers: Vec<String>,
    pub(crate) allow_credentials: bool,
    /// How long browsers may cache a preflight response.
    pub(crate) max_age_seconds: u64,
}

impl CorsConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.allowed_origins.is_empty() {
            return Err(Error::Configuration(
                "cors.allowed_origins must not be empty while CORS is enabled".to_owned(),
            ));
        }
        if self.allowed_methods.is_empty() {
            return Err(Error::Configuration(
                "cors.allowed_methods must not be empty while CORS is enabled".to_owned(),
            ));
        }
        let any_origin = self.allowed_origins.iter().any(|origin| origin == "*");
        if any_origin && self.allowed_origins.len() > 1 {
            return Err(Error::Configuration(
                "cors.allowed_origins cannot combine `*` with specific origins".to_owned(),
            ));
        }
        if self.allow_credentials
            && (any_origin
                || wildcard(&self.allowed_headers)
                || wildcard(&self.exposed_headers)
                || self.allowed_methods.iter().any(|method| method == "*"))
        {
            return Err(Error::Configuration(
                "cors.allow_credentials cannot be combined with `*` origins, methods, or headers"
                    .to_owned(),
            ));
        }
        for origin in self.allowed_origins.iter().filter(|origin| *origin != "*") {
            let valid = origin.split_once("://").is_some_and(|(scheme, host)| {
                matches!(scheme, "http" | "https")
                    && !host.is_empty()
                    && !host.contains(['/', '?', '#'])
            });
            if !valid {
                return Err(Error::Configuration(format!(
                    "cors.allowed_origins entry `{origin}` must look like `https://host[:port]`"
                )));
            }
        }
        Ok(())
    }
}

fn wildcard(values: &[String]) -> bool {
    values.iter().any(|value| value == "*")
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "HEAD".to_owned()],
            allowed_headers: ["range", "if-none-match", "if-range", "x-request-id"]
                .map(str::to_owned)
                .to_vec(),
            exposed_headers: [
                "content-length",
                "content-range",
                "accept-ranges",
                "etag",
                "x-request-id",
            ]
            .map(str::to_owned)
            .to_vec(),
            allow_credentials: false,
            max_age_seconds: 86_400,
        }
    }
}
