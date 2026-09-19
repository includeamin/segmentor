//! Configuration for asset resolution: the resolver choice, the mapper client, the registry
//! caches, and the policy for remote media.

use std::fmt;

use serde::Deserialize;

use crate::error::{Error, Result};

/// A credential that never appears in `Debug` output or logs.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    #[cfg(test)]
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// Where asset locations come from.
#[derive(Debug, Clone)]
pub(crate) enum ResolverSettings {
    /// The `[assets.*]` catalog in the configuration file.
    Static,
    /// An external mapper service.
    Http(MapperConfig),
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ResolverKind {
    #[default]
    Static,
    Http,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct RawResolver {
    #[serde(rename = "type", default)]
    pub(super) kind: ResolverKind,
    pub(super) http: Option<RawMapper>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RawMapper {
    base_url: String,
    bearer_token_env: Option<String>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    max_retries: u32,
    max_response_bytes: usize,
    default_ttl_ms: u64,
    min_ttl_ms: u64,
    max_ttl_ms: u64,
    negative_ttl_ms: u64,
    error_ttl_ms: u64,
    stale_if_error_ms: u64,
    readiness_probe_interval_ms: u64,
    allow_insecure_mapper: bool,
}

impl Default for RawMapper {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            bearer_token_env: None,
            connect_timeout_ms: 500,
            request_timeout_ms: 2000,
            max_retries: 2,
            max_response_bytes: 16 * 1024,
            default_ttl_ms: 300_000,
            min_ttl_ms: 5000,
            max_ttl_ms: 3_600_000,
            negative_ttl_ms: 5000,
            error_ttl_ms: 2000,
            stale_if_error_ms: 60_000,
            readiness_probe_interval_ms: 0,
            allow_insecure_mapper: false,
        }
    }
}

/// The validated mapper client settings.
#[derive(Debug, Clone)]
pub(crate) struct MapperConfig {
    pub(crate) base_url: String,
    pub(crate) bearer_token: Option<Secret>,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_retries: u32,
    pub(crate) max_response_bytes: usize,
    pub(crate) default_ttl_ms: u64,
    pub(crate) min_ttl_ms: u64,
    pub(crate) max_ttl_ms: u64,
    pub(crate) negative_ttl_ms: u64,
    pub(crate) error_ttl_ms: u64,
    pub(crate) stale_if_error_ms: u64,
    /// Zero disables the background reachability probe.
    pub(crate) readiness_probe_interval_ms: u64,
}

impl RawMapper {
    pub(super) fn validate(
        self,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<MapperConfig> {
        let fail = |message: &str| Err(Error::Configuration(message.to_owned()));
        let url = self.base_url.trim_end_matches('/');
        let scheme_ok = url.starts_with("https://")
            || (self.allow_insecure_mapper && url.starts_with("http://"));
        if url.is_empty() || !scheme_ok || url.contains(['?', '#']) {
            return fail(
                "resolver.http.base_url must be an https URL without a query (http requires allow_insecure_mapper)",
            );
        }
        if url.split_once("://").is_some_and(|(_, rest)| {
            rest.split('/')
                .next()
                .is_some_and(|authority| authority.contains('@'))
        }) {
            return fail("resolver.http.base_url must not contain credentials");
        }
        if self.connect_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.max_response_bytes == 0
            || self.default_ttl_ms == 0
            || self.min_ttl_ms == 0
            || self.max_ttl_ms == 0
            || self.negative_ttl_ms == 0
            || self.error_ttl_ms == 0
        {
            return fail("resolver.http timeouts, sizes, and TTLs must be greater than zero");
        }
        if !(self.min_ttl_ms <= self.default_ttl_ms && self.default_ttl_ms <= self.max_ttl_ms) {
            return fail("resolver.http requires min_ttl_ms <= default_ttl_ms <= max_ttl_ms");
        }
        let bearer_token = match self.bearer_token_env {
            None => None,
            Some(name) => Some(Secret(
                environment(&name)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        Error::Configuration(format!(
                            "resolver.http.bearer_token_env names `{name}`, which is not set"
                        ))
                    })?,
            )),
        };
        Ok(MapperConfig {
            base_url: url.to_owned(),
            bearer_token,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            max_retries: self.max_retries,
            max_response_bytes: self.max_response_bytes,
            default_ttl_ms: self.default_ttl_ms,
            min_ttl_ms: self.min_ttl_ms,
            max_ttl_ms: self.max_ttl_ms,
            negative_ttl_ms: self.negative_ttl_ms,
            error_ttl_ms: self.error_ttl_ms,
            stale_if_error_ms: self.stale_if_error_ms,
            readiness_probe_interval_ms: self.readiness_probe_interval_ms,
        })
    }
}

/// Caches and load scheduling for the asset registry.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RegistryConfig {
    /// Resolutions kept per asset ID.
    pub(crate) max_cached_resolutions: usize,
    /// How long a request waits for a load slot before `503`.
    pub(crate) load_queue_timeout_ms: u64,
    /// With the static resolver, load every asset before serving so a bad file fails startup.
    pub(crate) preload: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            max_cached_resolutions: 10_000,
            load_queue_timeout_ms: 5000,
            preload: true,
        }
    }
}

impl RegistryConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.max_cached_resolutions == 0 || self.load_queue_timeout_ms == 0 {
            return Err(Error::Configuration(
                "registry limits must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Policy and limits for media read from remote HTTP locations.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RemoteMediaConfig {
    /// Hosts a mapper may point at; empty means remote locations are refused.
    pub(crate) allowed_hosts: Vec<String>,
    /// Permit `http://` locations (development only).
    pub(crate) allow_insecure_http: bool,
    /// Permit locations that resolve to loopback, private, or link-local addresses.
    pub(crate) allow_private_addresses: bool,
    pub(crate) connect_timeout_ms: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_retries: u32,
    /// Simultaneous ranged requests to remote origins.
    pub(crate) max_inflight_reads: usize,
}

impl Default for RemoteMediaConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            allow_insecure_http: false,
            allow_private_addresses: false,
            connect_timeout_ms: 1000,
            request_timeout_ms: 10_000,
            max_retries: 2,
            max_inflight_reads: 64,
        }
    }
}

impl RemoteMediaConfig {
    pub(super) fn validate(&self) -> Result<()> {
        if self.connect_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.max_inflight_reads == 0
        {
            return Err(Error::Configuration(
                "remote_media timeouts and limits must be greater than zero".to_owned(),
            ));
        }
        if self
            .allowed_hosts
            .iter()
            .any(|host| host.is_empty() || host.contains(['/', ':', '@', ' ']))
        {
            return Err(Error::Configuration(
                "remote_media.allowed_hosts entries must be bare host names".to_owned(),
            ));
        }
        Ok(())
    }
}
