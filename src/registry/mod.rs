//! The asset registry: resolves asset IDs, loads their media on demand, and caches both.
//!
//! `AssetRegistry::get` is the one call the HTTP layer makes per request. The common case is a
//! cache hit that takes two short mutex sections and no I/O. On a miss the registry asks the
//! resolver where the asset lives, opens and parses it (bounded, off the async workers), and
//! keeps the result in a byte-weighted cache.
//!
//! Concurrent requests for one asset share a single resolve and load (single flight). That work
//! runs in its own task, so a client that disconnects mid-load does not cancel it for the others.
//! Failures are cached briefly so a broken asset or an unreachable mapper is not hammered.

mod cache;
mod opener;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, Semaphore};
use tokio::time::{Duration, timeout};

use crate::asset::PackagedAsset;
use crate::config::{Config, LimitsConfig, is_valid_asset_id};
use crate::error::{Error, Result};
use crate::observability::metrics::{CacheEvent, Metrics, ResolverOutcome};
use crate::resolver::{AssetResolver, Resolution, ResolveError, ResolvedAsset};
use cache::LoadedCache;
pub(crate) use opener::SourceOpener;

#[cfg(test)]
mod tests;

/// Why an asset could not be provided, in terms the HTTP layer maps to a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryError {
    /// No such asset: `404`.
    NotFound,
    /// A dependency is unavailable or overloaded: `503`.
    Unavailable(String),
    /// The mapper or media origin misbehaved or was refused: `502`.
    BadUpstream(String),
    /// The media exists but could not be loaded, for example unsupported codecs: `500`.
    LoadFailed(String),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RegistrySettings {
    pub(crate) segment_duration_ms: u64,
    pub(crate) max_cached_resolutions: usize,
    pub(crate) negative_ttl: Duration,
    pub(crate) error_ttl: Duration,
    pub(crate) stale_if_error: Duration,
    pub(crate) load_queue_timeout: Duration,
    pub(crate) max_concurrent_loads: usize,
    pub(crate) loaded_budget_bytes: u64,
}

#[derive(Debug)]
enum Slot {
    Found {
        resolved: ResolvedAsset,
        /// While set and in the future, a stale answer is served without asking the mapper.
        backoff_until: Option<Instant>,
    },
    Missing {
        until: Instant,
    },
}

#[derive(Debug, Default)]
struct Inner {
    resolutions: HashMap<String, Slot>,
    failures: HashMap<String, (Instant, RegistryError)>,
}

#[derive(Debug)]
pub(crate) struct AssetRegistry {
    resolver: AssetResolver,
    opener: SourceOpener,
    limits: LimitsConfig,
    settings: RegistrySettings,
    inner: Mutex<Inner>,
    loaded: Mutex<LoadedCache>,
    flights: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    load_slots: Semaphore,
    metrics: Arc<Metrics>,
}

/// What a lookup found without doing any work.
enum Lookup {
    Hit(Arc<PackagedAsset>),
    Failed(RegistryError),
    Missing,
    NeedsWork,
}

/// Holds one asset's single-flight lock and releases its map entry when nothing else waits.
struct Flight {
    registry: Arc<AssetRegistry>,
    id: String,
    lock: Option<Arc<AsyncMutex<()>>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        drop(self.guard.take());
        drop(self.lock.take());
        let mut flights = lock(&self.registry.flights);
        if flights
            .get(&self.id)
            .is_some_and(|entry| Arc::strong_count(entry) == 1)
        {
            flights.remove(&self.id);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No code path panics while holding these short critical sections; recover if one did.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AssetRegistry {
    pub(crate) fn new(
        resolver: AssetResolver,
        opener: SourceOpener,
        config: &Config,
        settings: RegistrySettings,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            resolver,
            opener,
            limits: config.limits.clone(),
            settings,
            inner: Mutex::new(Inner::default()),
            loaded: Mutex::new(LoadedCache::new(settings.loaded_budget_bytes)),
            flights: Mutex::new(HashMap::new()),
            load_slots: Semaphore::new(settings.max_concurrent_loads),
            metrics,
        }
    }

    /// Returns the loaded asset for `asset_id`, resolving and loading it if needed.
    pub(crate) async fn get(
        self: &Arc<Self>,
        asset_id: &str,
    ) -> std::result::Result<Arc<PackagedAsset>, RegistryError> {
        // Malformed IDs never reach the resolver, so path-like or oversized input is not forwarded.
        if !is_valid_asset_id(asset_id) {
            return Err(RegistryError::NotFound);
        }
        match self.lookup(asset_id) {
            Lookup::Hit(asset) => return Ok(asset),
            Lookup::Failed(error) => return Err(error),
            Lookup::Missing => return Err(RegistryError::NotFound),
            Lookup::NeedsWork => {}
        }

        let flight = self.enter_flight(asset_id).await;
        // Another request may have finished the work while this one waited for the lock.
        match self.lookup(asset_id) {
            Lookup::Hit(asset) => return Ok(asset),
            Lookup::Failed(error) => return Err(error),
            Lookup::Missing => return Err(RegistryError::NotFound),
            Lookup::NeedsWork => {}
        }
        let registry = Arc::clone(self);
        let id = asset_id.to_owned();
        tokio::spawn(async move {
            let _flight = flight;
            registry.resolve_and_load(&id).await
        })
        .await
        .map_err(|error| RegistryError::LoadFailed(format!("asset task failed: {error}")))?
    }

    /// Whether the resolver's backend is reachable, for readiness reporting.
    pub(crate) async fn resolver_healthy(&self) -> bool {
        self.resolver.healthy().await
    }

    /// Loads every asset the resolver already knows (the static catalog) so a bad file stops
    /// startup, and checks their combined index size against the memory budget.
    pub(crate) async fn preload(self: &Arc<Self>) -> Result<()> {
        let mut tasks = tokio::task::JoinSet::new();
        for id in self.resolver.known_ids() {
            let registry = Arc::clone(self);
            tasks.spawn(async move {
                let started = Instant::now();
                let result = registry.get(&id).await;
                (id, started, result)
            });
        }
        let mut total = 0u64;
        while let Some(joined) = tasks.join_next().await {
            let (id, started, result) = joined.map_err(|error| {
                Error::Configuration(format!("asset load task failed: {error}"))
            })?;
            let asset = result.map_err(|error| {
                Error::Configuration(format!(
                    "asset `{id}` could not be loaded: {}",
                    describe(&error)
                ))
            })?;
            total = total.saturating_add(asset.index_bytes());
            tracing::info!(
                event = "asset_loaded",
                asset.id = %id,
                media.tracks = asset.index.tracks.len(),
                media.segments = asset.plan.segments.len(),
                index.bytes = asset.index_bytes(),
                elapsed_ms = started.elapsed().as_millis(),
            );
        }
        if total > self.settings.loaded_budget_bytes {
            return Err(Error::Configuration(format!(
                "loaded asset indexes need {total} bytes, exceeding limits.max_index_bytes {}",
                self.settings.loaded_budget_bytes
            )));
        }
        Ok(())
    }

    async fn enter_flight(self: &Arc<Self>, asset_id: &str) -> Flight {
        let lock = Arc::clone(
            lock(&self.flights)
                .entry(asset_id.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        );
        let guard = if let Ok(guard) = Arc::clone(&lock).try_lock_owned() {
            guard
        } else {
            self.metrics.coalesced_waiter();
            Arc::clone(&lock).lock_owned().await
        };
        Flight {
            registry: Arc::clone(self),
            id: asset_id.to_owned(),
            lock: Some(lock),
            guard: Some(guard),
        }
    }

    /// Answers from caches only.
    fn lookup(&self, asset_id: &str) -> Lookup {
        let now = Instant::now();
        let (version, event) = {
            let mut inner = lock(&self.inner);
            if let Some((until, error)) = inner.failures.get(asset_id) {
                if now < *until {
                    return Lookup::Failed(error.clone());
                }
                inner.failures.remove(asset_id);
            }
            match inner.resolutions.get(asset_id) {
                Some(Slot::Missing { until }) if now < *until => {
                    self.metrics.resolution_event(CacheEvent::NegativeHit);
                    return Lookup::Missing;
                }
                Some(Slot::Found {
                    resolved,
                    backoff_until,
                }) => {
                    let fresh = now < resolved.valid_until;
                    let serving_stale = !fresh
                        && backoff_until.is_some_and(|until| now < until)
                        && self.stale_usable(resolved, now);
                    if !fresh && !serving_stale {
                        return Lookup::NeedsWork;
                    }
                    (
                        resolved.version.clone(),
                        if fresh {
                            CacheEvent::Hit
                        } else {
                            CacheEvent::Stale
                        },
                    )
                }
                _ => return Lookup::NeedsWork,
            }
        };
        match lock(&self.loaded).get(asset_id, &version) {
            Some(asset) => {
                self.metrics.resolution_event(event);
                Lookup::Hit(asset)
            }
            None => Lookup::NeedsWork,
        }
    }

    fn stale_usable(&self, resolved: &ResolvedAsset, now: Instant) -> bool {
        resolved.hard_expiry.is_none_or(|expiry| now < expiry)
            && now < resolved.valid_until + self.settings.stale_if_error
    }

    async fn resolve_and_load(
        &self,
        asset_id: &str,
    ) -> std::result::Result<Arc<PackagedAsset>, RegistryError> {
        let resolved = self.ensure_resolved(asset_id).await?;
        self.load(asset_id, &resolved).await
    }

    async fn ensure_resolved(
        &self,
        asset_id: &str,
    ) -> std::result::Result<ResolvedAsset, RegistryError> {
        let now = Instant::now();
        let known = match lock(&self.inner).resolutions.get(asset_id) {
            Some(Slot::Found { resolved, .. }) => Some(resolved.clone()),
            _ => None,
        };
        if let Some(resolved) = &known {
            if now < resolved.valid_until {
                self.metrics.resolution_event(CacheEvent::Hit);
                return Ok(resolved.clone());
            }
            self.metrics.resolution_event(CacheEvent::Revalidate);
        } else {
            self.metrics.resolution_event(CacheEvent::Miss);
        }

        // A location past its hard expiry is unusable, so ask for a fresh one, not "unchanged".
        let known_version = known
            .as_ref()
            .filter(|resolved| resolved.hard_expiry.is_none_or(|expiry| now < expiry))
            .map(|resolved| resolved.version.as_str());
        match self.resolver.resolve(asset_id, known_version).await {
            Ok(Resolution::Resolved(new)) => {
                self.metrics.resolver_result(ResolverOutcome::Ok);
                self.store_resolution(asset_id, &new, known.as_ref());
                Ok(new)
            }
            Ok(Resolution::Unchanged { valid_until }) => {
                self.metrics.resolver_result(ResolverOutcome::Unchanged);
                let Some(mut resolved) = known else {
                    return Err(RegistryError::BadUpstream(
                        "mapper answered unchanged for an unknown version".to_owned(),
                    ));
                };
                // Revalidation never extends a location's hard deadline.
                resolved.valid_until = resolved
                    .hard_expiry
                    .map_or(valid_until, |expiry| valid_until.min(expiry));
                self.put_slot(
                    asset_id,
                    Slot::Found {
                        resolved: resolved.clone(),
                        backoff_until: None,
                    },
                );
                Ok(resolved)
            }
            Err(ResolveError::NotFound) => {
                self.metrics.resolver_result(ResolverOutcome::NotFound);
                self.put_slot(
                    asset_id,
                    Slot::Missing {
                        until: Instant::now() + self.settings.negative_ttl,
                    },
                );
                lock(&self.loaded).retain_only(asset_id, None);
                self.publish_loaded();
                Err(RegistryError::NotFound)
            }
            Err(ResolveError::Unavailable(message)) => {
                self.metrics.resolver_result(ResolverOutcome::Unavailable);
                if let Some(resolved) = known.filter(|resolved| self.stale_usable(resolved, now)) {
                    tracing::warn!(
                        event = "resolve_stale_served",
                        asset.id = asset_id,
                        error = %message,
                    );
                    self.metrics.resolution_event(CacheEvent::Stale);
                    self.put_slot(
                        asset_id,
                        Slot::Found {
                            resolved: resolved.clone(),
                            backoff_until: Some(Instant::now() + self.settings.error_ttl),
                        },
                    );
                    return Ok(resolved);
                }
                self.remember_failure(asset_id, RegistryError::Unavailable(message.clone()));
                Err(RegistryError::Unavailable(message))
            }
            Err(ResolveError::Rejected(message)) => {
                self.metrics.resolver_result(ResolverOutcome::Rejected);
                tracing::error!(event = "resolve_rejected", asset.id = asset_id, error = %message);
                let error = RegistryError::BadUpstream(message);
                self.remember_failure(asset_id, error.clone());
                Err(error)
            }
        }
    }

    fn store_resolution(&self, asset_id: &str, new: &ResolvedAsset, old: Option<&ResolvedAsset>) {
        // A new version, or a new location for the same version (rotated signed URL), must not
        // keep serving from the previous source. There is no grace period for old versions.
        let changed =
            old.is_some_and(|old| old.version != new.version || old.location != new.location);
        if changed {
            lock(&self.loaded).retain_only(asset_id, None);
            self.publish_loaded();
        }
        self.put_slot(
            asset_id,
            Slot::Found {
                resolved: new.clone(),
                backoff_until: None,
            },
        );
    }

    fn put_slot(&self, asset_id: &str, slot: Slot) {
        let mut inner = lock(&self.inner);
        if inner.resolutions.len() >= self.settings.max_cached_resolutions
            && !inner.resolutions.contains_key(asset_id)
        {
            let now = Instant::now();
            inner.resolutions.retain(|_, slot| match slot {
                Slot::Found { resolved, .. } => now < resolved.valid_until,
                Slot::Missing { until } => now < *until,
            });
            if inner.resolutions.len() >= self.settings.max_cached_resolutions
                && let Some(victim) = inner.resolutions.keys().next().cloned()
            {
                inner.resolutions.remove(&victim);
            }
        }
        inner.resolutions.insert(asset_id.to_owned(), slot);
        inner.failures.remove(asset_id);
    }

    fn remember_failure(&self, asset_id: &str, error: RegistryError) {
        let mut inner = lock(&self.inner);
        if inner.failures.len() >= self.settings.max_cached_resolutions {
            let now = Instant::now();
            inner.failures.retain(|_, (until, _)| now < *until);
        }
        if inner.failures.len() < self.settings.max_cached_resolutions {
            inner.failures.insert(
                asset_id.to_owned(),
                (Instant::now() + self.settings.error_ttl, error),
            );
        }
    }

    async fn load(
        &self,
        asset_id: &str,
        resolved: &ResolvedAsset,
    ) -> std::result::Result<Arc<PackagedAsset>, RegistryError> {
        if let Some(asset) = lock(&self.loaded).get(asset_id, &resolved.version) {
            return Ok(asset);
        }
        let _slot = timeout(self.settings.load_queue_timeout, self.load_slots.acquire())
            .await
            .map_err(|_| RegistryError::Unavailable("asset load queue timed out".to_owned()))?
            .map_err(|_| RegistryError::Unavailable("asset loading is shut down".to_owned()))?;

        let started = Instant::now();
        let result = async {
            let source = self.opener.open(&resolved.location).await?;
            PackagedAsset::load(source, self.settings.segment_duration_ms, &self.limits).await
        }
        .await;
        self.metrics.asset_load(result.is_ok(), started.elapsed());
        match result {
            Ok(asset) => {
                let asset = Arc::new(asset);
                let mut loaded = lock(&self.loaded);
                // Only one version of an asset is ever resident.
                loaded.retain_only(asset_id, Some(&resolved.version));
                loaded.insert(asset_id, &resolved.version, Arc::clone(&asset));
                drop(loaded);
                self.publish_loaded();
                tracing::debug!(
                    event = "asset_loaded",
                    asset.id = asset_id,
                    index.bytes = asset.index_bytes(),
                    elapsed_ms = started.elapsed().as_millis(),
                );
                Ok(asset)
            }
            Err(error) => {
                tracing::error!(event = "asset_load_failed", asset.id = asset_id, %error);
                let error = map_load_error(error);
                self.remember_failure(asset_id, error.clone());
                Err(error)
            }
        }
    }

    fn publish_loaded(&self) {
        let loaded = lock(&self.loaded);
        self.metrics.set_loaded(loaded.len(), loaded.weight());
    }
}

fn map_load_error(error: Error) -> RegistryError {
    match error {
        Error::UpstreamUnavailable(message) => RegistryError::Unavailable(message),
        Error::Upstream(message) => RegistryError::BadUpstream(message),
        other => RegistryError::LoadFailed(other.to_string()),
    }
}

fn describe(error: &RegistryError) -> String {
    match error {
        RegistryError::NotFound => "not found".to_owned(),
        RegistryError::Unavailable(message)
        | RegistryError::BadUpstream(message)
        | RegistryError::LoadFailed(message) => message.clone(),
    }
}
