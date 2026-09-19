//! Asset resolution: turning an asset ID into the location of its media.
//!
//! A resolver answers one question and never opens media: given an ID, where does the file live
//! and which version of it is current? The static resolver answers from the configuration file;
//! the mapper resolver asks an external service over HTTP. The registry caches answers and loads
//! the media they point at.

mod catalog;
mod mapper;
mod policy;

use std::path::PathBuf;
use std::time::Instant;

use reqwest::Url;

pub(crate) use catalog::StaticResolver;
pub(crate) use mapper::HttpResolver;
pub(crate) use policy::LocationPolicy;

/// Where an asset's media lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssetLocation {
    /// A path beneath `storage.media_root`, relative or already canonical.
    File(PathBuf),
    /// An HTTP(S) URL serving the MP4 with byte-range support.
    Http(Url),
}

/// A resolver's answer for one asset.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAsset {
    pub(crate) location: AssetLocation,
    /// Opaque change token: equal versions mean identical media.
    pub(crate) version: String,
    /// After this instant the answer must be revalidated before use.
    pub(crate) valid_until: Instant,
    /// After this instant the location itself is dead (for example a signed URL) and must never
    /// be used, even as a stale fallback.
    pub(crate) hard_expiry: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// The resolver definitively says the asset does not exist.
    NotFound,
    /// The resolver could not be reached, timed out, or is shedding load.
    Unavailable(String),
    /// The resolver answered, but the answer is invalid, forbidden by policy, or unauthorized.
    Rejected(String),
}

#[derive(Debug)]
pub(crate) enum Resolution {
    /// A new or changed answer.
    Resolved(ResolvedAsset),
    /// The known version is still current.
    Unchanged { valid_until: Instant },
}

#[derive(Debug)]
pub(crate) enum AssetResolver {
    Static(StaticResolver),
    Http(HttpResolver),
}

impl AssetResolver {
    /// Resolves `asset_id`. `known_version` lets a mapper answer "unchanged" cheaply.
    pub(crate) async fn resolve(
        &self,
        asset_id: &str,
        known_version: Option<&str>,
    ) -> Result<Resolution, ResolveError> {
        match self {
            Self::Static(resolver) => resolver.resolve(asset_id),
            Self::Http(resolver) => resolver.resolve(asset_id, known_version).await,
        }
    }

    /// Whether the resolver's backend answers a health check. Always true when there is none.
    pub(crate) async fn healthy(&self) -> bool {
        match self {
            Self::Static(_) => true,
            Self::Http(resolver) => resolver.healthy().await,
        }
    }

    /// Asset IDs known without asking a remote service (only the static catalog has them).
    pub(crate) fn known_ids(&self) -> Vec<String> {
        match self {
            Self::Static(resolver) => resolver.ids(),
            Self::Http(_) => Vec::new(),
        }
    }
}
