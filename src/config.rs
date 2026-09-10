use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: SocketAddr,
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
    assets: BTreeMap<String, AssetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LimitsConfig {
    pub(crate) max_assets: usize,
    pub(crate) max_source_bytes: u64,
    pub(crate) max_metadata_bytes: u64,
    pub(crate) max_tracks: usize,
    pub(crate) max_samples_per_track: usize,
    pub(crate) max_samples_per_segment: usize,
    pub(crate) max_segment_bytes: u64,
    pub(crate) max_segment_jobs: usize,
    pub(crate) segment_queue_timeout_ms: u64,
    pub(crate) stream_chunk_bytes: usize,
    pub(crate) max_request_header_bytes: usize,
    pub(crate) request_timeout_ms: u64,
    pub(crate) max_startup_parses: usize,
}

impl LimitsConfig {
    fn validate(&self) -> Result<()> {
        if self.max_assets == 0
            || self.max_source_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_tracks == 0
            || self.max_samples_per_track == 0
            || self.max_samples_per_segment == 0
            || self.max_segment_bytes == 0
            || self.max_segment_jobs == 0
            || self.segment_queue_timeout_ms == 0
            || self.stream_chunk_bytes == 0
            || self.max_request_header_bytes == 0
            || self.request_timeout_ms == 0
            || self.max_startup_parses == 0
        {
            return Err(Error::Configuration(
                "all resource limits must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_assets: 1000,
            max_source_bytes: 1024 * 1024 * 1024 * 1024,
            max_metadata_bytes: 64 * 1024 * 1024,
            max_tracks: 8,
            max_samples_per_track: 2_000_000,
            max_samples_per_segment: 100_000,
            max_segment_bytes: 64 * 1024 * 1024,
            max_segment_jobs: default_segment_jobs(),
            segment_queue_timeout_ms: 2000,
            stream_chunk_bytes: 256 * 1024,
            max_request_header_bytes: 16 * 1024,
            request_timeout_ms: 30_000,
            max_startup_parses: 4,
        }
    }
}

fn default_segment_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |parallelism| parallelism.get().saturating_mul(2).min(32))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LoggingConfig {
    pub(crate) level: LogLevel,
    pub(crate) format: LogFormat,
    pub(crate) buffer_capacity: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            format: LogFormat::Json,
            buffer_capacity: 8192,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    Compact,
    Json,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    listen: SocketAddr,
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
}
