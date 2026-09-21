//! The resolver backed by the `[assets.*]` tables in the configuration file.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{AssetLocation, Resolution, ResolveError, ResolvedAsset};

/// The static catalog never changes while the process runs, so its answers never expire and its
/// one version is a constant.
const STATIC_VERSION: &str = "static";
const FOREVER: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

#[derive(Debug)]
pub(crate) struct StaticResolver {
    assets: BTreeMap<String, PathBuf>,
}

impl StaticResolver {
    pub(crate) fn new(assets: BTreeMap<String, PathBuf>) -> Self {
        Self { assets }
    }

    pub(crate) fn ids(&self) -> Vec<String> {
        self.assets.keys().cloned().collect()
    }

    pub(crate) fn resolve(&self, asset_id: &str) -> Result<Resolution, ResolveError> {
        let path = self.assets.get(asset_id).ok_or(ResolveError::NotFound)?;
        Ok(Resolution::Resolved(ResolvedAsset {
            location: AssetLocation::File(path.clone()),
            subtitles: Vec::new(),
            version: STATIC_VERSION.to_owned(),
            valid_until: Instant::now() + FOREVER,
            hard_expiry: None,
        }))
    }
}
