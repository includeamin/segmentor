use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

mod cors;
mod limits;
mod logging;

pub(crate) use cors::CorsConfig;
pub(crate) use limits::LimitsConfig;
pub(crate) use logging::{LogFormat, LoggingConfig};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
    pub(crate) shutdown_delay_ms: u64,
    pub(crate) shutdown_grace_ms: u64,
    pub(crate) cors: CorsConfig,
    pub(crate) segment_duration_ms: u64,
    pub(crate) assets: BTreeMap<String, PathBuf>,
    pub(crate) logging: LoggingConfig,
    pub(crate) limits: LimitsConfig,
}

impl Config {
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)?;
        let config_directory = path.parent().unwrap_or_else(|| Path::new("."));
        Self::parse(&contents, config_directory)
    }

    fn parse(contents: &str, config_directory: &Path) -> Result<Self> {
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
        if assets.is_empty() {
            return Err(Error::Configuration(
                "at least one asset must be configured".to_owned(),
            ));
        }

        Ok(Self {
            listen: raw.server.listen,
            shutdown_delay_ms: raw.server.shutdown_delay_ms,
            shutdown_grace_ms: raw.server.shutdown_grace_ms,
            cors: raw.cors,
            segment_duration_ms: raw.packaging.segment_duration_ms,
            assets,
            logging: raw.logging,
            limits: raw.limits,
        })
    }
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
}
