//! The loaded-asset cache: a byte-weighted LRU.
//!
//! A parsed asset is dominated by its sample index (about 40 bytes per sample), so an entry
//! count cannot bound memory. Each entry is weighted by `PackagedAsset::index_bytes` and the
//! least recently used entries are dropped when the total exceeds the budget. Requests that
//! already hold an `Arc` keep working after eviction.

use std::collections::HashMap;
use std::sync::Arc;

use crate::composite::ServedAsset;

/// One cached asset, as reported to the admin status endpoint.
#[derive(Debug, Clone)]
pub(crate) struct CachedAsset {
    pub(crate) asset_id: String,
    pub(crate) version: String,
    pub(crate) bytes: u64,
    pub(crate) tracks: usize,
    pub(crate) subtitles: usize,
    pub(crate) duration_seconds: f64,
}

type Key = (String, String);

#[derive(Debug)]
struct Entry {
    asset: Arc<ServedAsset>,
    weight: u64,
    last_used: u64,
}

#[derive(Debug)]
pub(crate) struct LoadedCache {
    entries: HashMap<Key, Entry>,
    total_weight: u64,
    budget: u64,
    clock: u64,
}

impl LoadedCache {
    pub(crate) fn new(budget: u64) -> Self {
        Self {
            entries: HashMap::new(),
            total_weight: 0,
            budget,
            clock: 0,
        }
    }

    pub(crate) fn get(&mut self, asset_id: &str, version: &str) -> Option<Arc<ServedAsset>> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self
            .entries
            .get_mut(&(asset_id.to_owned(), version.to_owned()))?;
        entry.last_used = clock;
        Some(Arc::clone(&entry.asset))
    }

    /// Stores an asset, evicting the least recently used others while over budget.
    ///
    /// An asset heavier than the whole budget is not retained at all: the caller still serves
    /// the request from its `Arc`, and the next request reloads it.
    pub(crate) fn insert(&mut self, asset_id: &str, version: &str, asset: Arc<ServedAsset>) {
        let weight = asset.index_bytes();
        if weight > self.budget {
            return;
        }
        self.clock += 1;
        let key = (asset_id.to_owned(), version.to_owned());
        if let Some(previous) = self.entries.remove(&key) {
            self.total_weight -= previous.weight;
        }
        while self.total_weight + weight > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.total_weight -= evicted.weight;
            }
        }
        self.total_weight += weight;
        self.entries.insert(
            key,
            Entry {
                asset,
                weight,
                last_used: self.clock,
            },
        );
    }

    /// Drops every version of `asset_id` except `keep`, or all of them when `keep` is `None`.
    pub(crate) fn retain_only(&mut self, asset_id: &str, keep: Option<&str>) {
        let stale = self
            .entries
            .keys()
            .filter(|(id, version)| id == asset_id && Some(version.as_str()) != keep)
            .cloned()
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(entry) = self.entries.remove(&key) {
                self.total_weight -= entry.weight;
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn weight(&self) -> u64 {
        self.total_weight
    }

    pub(crate) fn budget(&self) -> u64 {
        self.budget
    }

    /// Every cached asset, most recently used first.
    pub(crate) fn snapshot(&self) -> Vec<CachedAsset> {
        let mut rows = self
            .entries
            .iter()
            .map(|((asset_id, version), entry)| {
                (
                    entry.last_used,
                    CachedAsset {
                        asset_id: asset_id.clone(),
                        version: version.clone(),
                        bytes: entry.weight,
                        tracks: entry.asset.track_count(),
                        subtitles: entry.asset.subtitle_count(),
                        duration_seconds: entry.asset.duration_seconds(),
                    },
                )
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|(last_used, _)| std::cmp::Reverse(*last_used));
        rows.into_iter().map(|(_, row)| row).collect()
    }
}
