use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

mod cors;
mod limits;
mod logging;
mod resolver;

pub(crate) use cors::CorsConfig;
pub(crate) use limits::LimitsConfig;
pub(crate) use logging::{LogFormat, LoggingConfig};
pub(crate) use resolver::{
    MapperConfig, RegistryConfig, RemoteMediaConfig, ResolverSettings, Secret,
};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) shutdown_delay_ms: u64,
    pub(crate) shutdown_grace_ms: u64,
    pub(crate) cors: CorsConfig,
    pub(crate) segment_duration_ms: u64,
    pub(crate) assets: BTreeMap<String, PathBuf>,
    pub(crate) media_root: PathBuf,
    pub(crate) resolver: ResolverSettings,
    pub(crate) registry: RegistryConfig,
    pub(crate) remote_media: RemoteMediaConfig,
    pub(crate) logging: LoggingConfig,
    pub(crate) limits: LimitsConfig,
}

impl Config {
    /// A static-catalog configuration assembled in code, for tests and benchmarks. Assets are
    /// canonical absolute paths beneath `media_root`.
    pub(crate) fn for_catalog(
        media_root: PathBuf,
        assets: BTreeMap<String, PathBuf>,
        segment_duration_ms: u64,
        limits: LimitsConfig,
    ) -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            shutdown_delay_ms: 0,
            shutdown_grace_ms: 1000,
            cors: CorsConfig::default(),
            segment_duration_ms,
            assets,
            media_root,
            resolver: ResolverSettings::Static,
            registry: RegistryConfig::default(),
            remote_media: RemoteMediaConfig::default(),
            logging: LoggingConfig::default(),
            limits,
        }
    }

    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)?;
        let config_directory = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&contents, config_directory)
    }

    fn parse(contents: &str, config_directory: &Path) -> Result<Self> {
        Self::parse_with(contents, config_directory, &|name| std::env::var(name).ok())
    }

    /// Parses with an injectable environment so secrets can be tested without touching the
    /// process environment.
    fn parse_with(
        contents: &str,
        config_directory: &Path,
        environment: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents)?;
        if raw.packaging.segment_duration_ms == 0 {
            return Err(Error::Configuration(
                "packaging.segment_duration_ms must be greater than zero".to_owned(),
            ));
        }
        if raw.logging.buffer_capacity == 0 {
            return Err(Error::Configuration(
                "logging.buffer_capacity must be greater than zero".to_owned(),
            ));
        }
        raw.limits.validate()?;
        raw.server.validate()?;
        raw.cors.validate()?;
        raw.registry.validate()?;
        raw.remote_media.validate()?;
        if raw.assets.len() > raw.limits.max_assets {
            return Err(Error::Configuration(format!(
                "asset count exceeds configured limit {}",
                raw.limits.max_assets
            )));
        }

        let media_root = if raw.storage.media_root.is_absolute() {
            raw.storage.media_root
        } else {
            config_directory.join(raw.storage.media_root)
        }
        .canonicalize()?;
        if !media_root.is_dir() {
            return Err(Error::Configuration(
                "storage.media_root must be a directory".to_owned(),
            ));
        }

        let mut assets = BTreeMap::new();
        for (asset_id, asset) in raw.assets {
            validate_asset_id(&asset_id)?;
            if asset.path.is_absolute() {
                return Err(Error::Configuration(format!(
                    "asset `{asset_id}` path must be relative to storage.media_root"
                )));
            }
            let source = media_root.join(asset.path).canonicalize()?;
            if !source.starts_with(&media_root) || !source.is_file() {
                return Err(Error::Configuration(format!(
                    "asset `{asset_id}` must resolve to a file beneath storage.media_root"
                )));
            }
            assets.insert(asset_id, source);
        }
        let resolver = match (raw.resolver.kind, raw.resolver.http) {
            (resolver::ResolverKind::Static, None) => {
                if assets.is_empty() {
                    return Err(Error::Configuration(
                        "at least one asset must be configured".to_owned(),
                    ));
                }
                ResolverSettings::Static
            }
            (resolver::ResolverKind::Static, Some(_)) => {
                return Err(Error::Configuration(
                    "resolver.http requires resolver.type = \"http\"".to_owned(),
                ));
            }
            (resolver::ResolverKind::Http, None) => {
                return Err(Error::Configuration(
                    "resolver.type = \"http\" requires a [resolver.http] table".to_owned(),
                ));
            }
            (resolver::ResolverKind::Http, Some(mapper)) => {
                if !assets.is_empty() {
                    return Err(Error::Configuration(
                        "[assets] cannot be combined with resolver.type = \"http\"".to_owned(),
                    ));
                }
                ResolverSettings::Http(mapper.validate(environment)?)
            }
        };

        Ok(Self {
            listen: raw.server.listen,
            shutdown_delay_ms: raw.server.shutdown_delay_ms,
            shutdown_grace_ms: raw.server.shutdown_grace_ms,
            cors: raw.cors,
            segment_duration_ms: raw.packaging.segment_duration_ms,
            assets,
            media_root,
            resolver,
            registry: raw.registry,
            remote_media: raw.remote_media,
            logging: raw.logging,
            limits: raw.limits,
        })
    }
}

/// Whether `asset_id` is 1 to 128 ASCII letters, digits, `-`, or `_`.
pub(crate) fn is_valid_asset_id(asset_id: &str) -> bool {
    !asset_id.is_empty()
        && asset_id.len() <= 128
        && asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_asset_id(asset_id: &str) -> Result<()> {
    let valid = !asset_id.is_empty()
        && asset_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err(Error::Configuration(format!(
            "asset ID `{asset_id}` must contain only ASCII letters, digits, '-' or '_'"
        )))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: ServerConfig,
    storage: StorageConfig,
    #[serde(default)]
    packaging: PackagingConfig,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    limits: LimitsConfig,
    #[serde(default)]
    cors: CorsConfig,
    #[serde(default)]
    resolver: resolver::RawResolver,
    #[serde(default)]
    registry: RegistryConfig,
    #[serde(default)]
    remote_media: RemoteMediaConfig,
    #[serde(default)]
    assets: BTreeMap<String, AssetConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    listen: SocketAddr,
    #[serde(default)]
    shutdown_delay_ms: u64,
    #[serde(default = "default_shutdown_grace_ms")]
    shutdown_grace_ms: u64,
}

impl ServerConfig {
    fn validate(&self) -> Result<()> {
        if self.shutdown_grace_ms == 0 {
            return Err(Error::Configuration(
                "server.shutdown_grace_ms must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

const fn default_shutdown_grace_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageConfig {
    media_root: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagingConfig {
    #[serde(default = "default_segment_duration_ms")]
    segment_duration_ms: u64,
}

impl Default for PackagingConfig {
    fn default() -> Self {
        Self {
            segment_duration_ms: default_segment_duration_ms(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetConfig {
    path: PathBuf,
}

const fn default_segment_duration_ms() -> u64 {
    6000
}

#[cfg(test)]
mod tests {
    use super::logging::LogLevel;
    use super::*;

    fn fixture_directory() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    #[test]
    fn resolves_an_asset_beneath_the_media_root() {
        let config = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"

                [storage]
                media_root = "."

                [packaging]
                segment_duration_ms = 1000

                [assets.sample]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect("configuration should be valid");

        assert_eq!(config.listen, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.segment_duration_ms, 1000);
        assert_eq!(config.assets.len(), 1);
        assert_eq!(config.logging.level, LogLevel::Info);
        assert_eq!(config.logging.format, LogFormat::Json);
        assert_eq!(config.limits.max_tracks, 8);
        assert!(config.assets["sample"].ends_with("tests/fixtures/h264-aac.mp4"));
    }

    #[test]
    fn rejects_asset_ids_that_are_not_url_safe() {
        let error = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [assets."../sample"]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect_err("unsafe asset ID should fail");

        assert!(error.to_string().contains("asset ID"));
    }

    #[test]
    fn rejects_asset_paths_outside_the_media_root() {
        let error = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [assets.sample]
                path = "../../Cargo.toml"
            "#,
            &fixture_directory(),
        )
        .expect_err("escaping path should fail");

        assert!(error.to_string().contains("beneath storage.media_root"));
    }

    #[test]
    fn parses_logging_configuration() {
        let config = Config::parse(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [logging]
                level = "debug"
                format = "compact"
                buffer_capacity = 1024
                [assets.sample]
                path = "h264-aac.mp4"
            "#,
            &fixture_directory(),
        )
        .expect("logging configuration should be valid");

        assert_eq!(config.logging.level, LogLevel::Debug);
        assert_eq!(config.logging.format, LogFormat::Compact);
        assert_eq!(config.logging.buffer_capacity, 1024);
    }

    fn parse_with(extra: &str) -> Result<Config> {
        Config::parse(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    {extra}
                    [assets.sample]
                    path = "h264-aac.mp4"
                "#
            ),
            &fixture_directory(),
        )
    }

    #[test]
    fn cors_defaults_allow_any_origin_and_expose_range_headers() {
        let config = parse_with("").expect("defaults should be valid");

        assert!(config.cors.enabled);
        assert_eq!(config.cors.allowed_origins, ["*"]);
        assert!(
            config
                .cors
                .exposed_headers
                .iter()
                .any(|name| name == "content-range")
        );
        assert_eq!(config.shutdown_grace_ms, 30_000);
    }

    #[test]
    fn parses_explicit_cors_policy() {
        let config = parse_with(
            r#"
            [cors]
            allowed_origins = ["https://player.example.com", "http://localhost:5173"]
            allowed_headers = ["range"]
            allow_credentials = true
            max_age_seconds = 600
            "#,
        )
        .expect("explicit CORS policy should be valid");

        assert_eq!(config.cors.allowed_origins.len(), 2);
        assert!(config.cors.allow_credentials);
        assert_eq!(config.cors.max_age_seconds, 600);
    }

    #[test]
    fn rejects_invalid_cors_policies() {
        for extra in [
            "[cors]\nallowed_origins = []",
            "[cors]\nallowed_origins = [\"*\", \"https://a.example\"]",
            "[cors]\nallow_credentials = true",
            "[cors]\nallowed_origins = [\"https://a.example/path\"]",
            "[cors]\nallowed_origins = [\"a.example\"]",
        ] {
            parse_with(extra).expect_err(extra);
        }
    }

    #[test]
    fn disabled_cors_skips_validation() {
        parse_with("[cors]\nenabled = false\nallowed_origins = []")
            .expect("disabled CORS should not be validated");
    }

    #[test]
    fn parses_an_http_resolver_without_assets() {
        let config = parse_with(
            r#"
            [resolver]
            type = "http"
            [resolver.http]
            base_url = "https://mapper.example.net/"
            "#,
        );
        // parse_with adds a static asset; the two forms are mutually exclusive.
        assert!(config.is_err());

        let config = Config::parse_with(
            r#"
                [server]
                listen = "127.0.0.1:8080"
                [storage]
                media_root = "."
                [resolver]
                type = "http"
                [resolver.http]
                base_url = "https://mapper.example.net/"
                bearer_token_env = "VOD_TOKEN"
                [remote_media]
                allowed_hosts = ["origin.example.net"]
            "#,
            &fixture_directory(),
            &|name| (name == "VOD_TOKEN").then(|| "secret-value".to_owned()),
        )
        .expect("http resolver configuration should be valid");

        let ResolverSettings::Http(mapper) = &config.resolver else {
            panic!("expected the http resolver");
        };
        assert_eq!(mapper.base_url, "https://mapper.example.net");
        assert_eq!(
            mapper.bearer_token.as_ref().unwrap().expose(),
            "secret-value"
        );
        assert!(
            !format!("{mapper:?}").contains("secret-value"),
            "tokens must be redacted"
        );
        assert!(config.assets.is_empty());
        assert_eq!(config.remote_media.allowed_hosts, ["origin.example.net"]);
    }

    fn mapper_config(extra: &str) -> Result<Config> {
        Config::parse_with(
            &format!(
                r#"
                    [server]
                    listen = "127.0.0.1:8080"
                    [storage]
                    media_root = "."
                    [resolver]
                    type = "http"
                    [resolver.http]
                    {extra}
                "#
            ),
            &fixture_directory(),
            &|_| None,
        )
    }

    #[test]
    fn rejects_invalid_mapper_settings() {
        for extra in [
            "",
            "base_url = \"http://mapper.example.net\"",
            "base_url = \"https://user:pw@mapper.example.net\"",
            "base_url = \"https://mapper.example.net?x=1\"",
            "base_url = \"https://m.example.net\"\nrequest_timeout_ms = 0",
            "base_url = \"https://m.example.net\"\nmin_ttl_ms = 999999999",
            "base_url = \"https://m.example.net\"\nbearer_token_env = \"UNSET_VARIABLE\"",
        ] {
            mapper_config(extra).expect_err(extra);
        }
        mapper_config("base_url = \"http://localhost:9\"\nallow_insecure_mapper = true")
            .expect("insecure mapper is allowed when opted in");
    }

    #[test]
    fn rejects_inconsistent_resolver_sections() {
        parse_with(
            "[resolver]\ntype = \"static\"\n[resolver.http]\nbase_url = \"https://m.example.net\"",
        )
        .expect_err("http table needs the http type");
        Config::parse_with(
            "[server]\nlisten = \"127.0.0.1:8080\"\n[storage]\nmedia_root = \".\"\n[resolver]\ntype = \"http\"",
            &fixture_directory(),
            &|_| None,
        )
        .expect_err("http type needs its table");
    }

    #[test]
    fn rejects_invalid_remote_media_hosts() {
        parse_with("[remote_media]\nallowed_hosts = [\"https://origin.example.net/path\"]")
            .expect_err("hosts must be bare");
        parse_with("[remote_media]\nmax_inflight_reads = 0").expect_err("limits must be positive");
    }
}
