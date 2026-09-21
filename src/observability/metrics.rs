//! Lock-free Prometheus metrics for the HTTP origin.
//!
//! Every series is a fixed-size array of atomics indexed by a small closed set of route and
//! status values, so recording a request never allocates or takes a lock and label cardinality
//! cannot grow with client input.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Statuses tracked individually; anything else is folded into the final `other` slot.
const STATUSES: [u16; 12] = [200, 206, 304, 400, 404, 408, 416, 431, 500, 502, 503, 0];

/// Upper bounds in seconds for the response-time histogram.
const BUCKETS: [f64; 13] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

#[derive(Debug)]
pub(crate) struct Metrics {
    /// Route templates supplied by the router; two extra slots follow them for `unmatched`
    /// (no route matched) and `other` (a template the router did not declare).
    routes: &'static [&'static str],
    requests: Vec<[AtomicU64; STATUSES.len()]>,
    duration_buckets: Vec<[AtomicU64; BUCKETS.len() + 1]>,
    duration_sum_micros: Vec<AtomicU64>,
    in_flight: AtomicI64,
    connections_open: AtomicI64,
    resolver_outcomes: [AtomicU64; RESOLVER_OUTCOMES.len()],
    cache_events: [AtomicU64; CACHE_EVENTS.len()],
    loads_ok: AtomicU64,
    loads_failed: AtomicU64,
    load_micros: AtomicU64,
    coalesced_waiters: AtomicU64,
    location_rotations: AtomicU64,
    loaded_assets: AtomicU64,
    loaded_bytes: AtomicU64,
    connections_rejected: AtomicU64,
    response_bytes: AtomicU64,
    source_read_bytes: AtomicU64,
    shed_requests: AtomicU64,
    segment_queue_timeouts: AtomicU64,
    stream_aborts_idle: AtomicU64,
    stream_aborts_client: AtomicU64,
    stream_aborts_error: AtomicU64,
}

impl Metrics {
    /// Creates metrics labelled by the given route templates.
    ///
    /// The router owns the list, so a new route is registered and counted from one definition.
    pub(crate) fn new(routes: &'static [&'static str]) -> Self {
        let slots = routes.len() + 2;
        Self {
            routes,
            requests: (0..slots)
                .map(|_| std::array::from_fn(|_| AtomicU64::new(0)))
                .collect(),
            duration_buckets: (0..slots)
                .map(|_| std::array::from_fn(|_| AtomicU64::new(0)))
                .collect(),
            duration_sum_micros: (0..slots).map(|_| AtomicU64::new(0)).collect(),
            in_flight: AtomicI64::new(0),
            connections_open: AtomicI64::new(0),
            resolver_outcomes: std::array::from_fn(|_| AtomicU64::new(0)),
            cache_events: std::array::from_fn(|_| AtomicU64::new(0)),
            loads_ok: AtomicU64::new(0),
            loads_failed: AtomicU64::new(0),
            load_micros: AtomicU64::new(0),
            coalesced_waiters: AtomicU64::new(0),
            location_rotations: AtomicU64::new(0),
            loaded_assets: AtomicU64::new(0),
            loaded_bytes: AtomicU64::new(0),
            connections_rejected: AtomicU64::new(0),
            response_bytes: AtomicU64::new(0),
            source_read_bytes: AtomicU64::new(0),
            shed_requests: AtomicU64::new(0),
            segment_queue_timeouts: AtomicU64::new(0),
            stream_aborts_idle: AtomicU64::new(0),
            stream_aborts_client: AtomicU64::new(0),
            stream_aborts_error: AtomicU64::new(0),
        }
    }

    fn unmatched(&self) -> usize {
        self.routes.len()
    }

    fn other(&self) -> usize {
        self.routes.len() + 1
    }

    fn labels(&self) -> impl Iterator<Item = &'static str> {
        self.routes.iter().copied().chain(["unmatched", "other"])
    }
}

/// Decrements the in-flight gauge when dropped.
#[derive(Debug)]
pub(crate) struct InFlight(Arc<Metrics>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Relaxed);
    }
}

/// The result of one call to the asset resolver.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResolverOutcome {
    Ok,
    Unchanged,
    NotFound,
    Unavailable,
    Rejected,
}

const RESOLVER_OUTCOMES: [&str; 5] = ["ok", "unchanged", "not_found", "unavailable", "rejected"];

/// How a request found (or did not find) a cached resolution.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CacheEvent {
    Hit,
    Miss,
    Revalidate,
    Stale,
    NegativeHit,
}

const CACHE_EVENTS: [&str; 5] = ["hit", "miss", "revalidate", "stale", "negative_hit"];

/// Decrements the open-connection gauge when dropped.
#[derive(Debug)]
pub(crate) struct OpenConnection(Arc<Metrics>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.connections_open.fetch_sub(1, Relaxed);
    }
}

/// Why a streaming media response ended before every byte was sent.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StreamAbort {
    /// The client stopped reading for longer than the idle timeout.
    Idle,
    /// The client disconnected.
    Client,
    /// A source read or permit acquisition failed.
    Error,
}

impl Metrics {
    /// Index for a matched route template, `unmatched` for a request that matched no route, or
    /// `other` for a template the router did not declare.
    pub(crate) fn route_index(&self, matched_path: Option<&str>) -> usize {
        matched_path.map_or_else(
            || self.unmatched(),
            |path| {
                self.routes
                    .iter()
                    .position(|route| *route == path)
                    .unwrap_or_else(|| self.other())
            },
        )
    }

    /// Counts a request as in flight until the returned guard is dropped, including when the
    /// request future is cancelled by a client disconnect.
    pub(crate) fn request_started(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Relaxed);
        InFlight(Arc::clone(self))
    }

    /// Counts a connection as open until the returned guard is dropped.
    pub(crate) fn connection_opened(self: &Arc<Self>) -> OpenConnection {
        self.connections_open.fetch_add(1, Relaxed);
        OpenConnection(Arc::clone(self))
    }

    pub(crate) fn resolver_result(&self, outcome: ResolverOutcome) {
        self.resolver_outcomes[outcome as usize].fetch_add(1, Relaxed);
    }

    pub(crate) fn resolution_event(&self, event: CacheEvent) {
        self.cache_events[event as usize].fetch_add(1, Relaxed);
    }

    pub(crate) fn asset_load(&self, succeeded: bool, elapsed: Duration) {
        if succeeded {
            &self.loads_ok
        } else {
            &self.loads_failed
        }
        .fetch_add(1, Relaxed);
        self.load_micros.fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Relaxed,
        );
    }

    /// A request that waited on another request's in-flight resolve or load.
    pub(crate) fn coalesced_waiter(&self) {
        self.coalesced_waiters.fetch_add(1, Relaxed);
    }

    /// A loaded asset's signed URL was replaced in place, without reloading it.
    pub(crate) fn location_rotated(&self) {
        self.location_rotations.fetch_add(1, Relaxed);
    }

    pub(crate) fn set_loaded(&self, assets: usize, bytes: u64) {
        self.loaded_assets.store(assets as u64, Relaxed);
        self.loaded_bytes.store(bytes, Relaxed);
    }

    pub(crate) fn connection_rejected(&self) {
        self.connections_rejected.fetch_add(1, Relaxed);
    }

    pub(crate) fn request_finished(&self, route: usize, status: u16, elapsed: Duration) {
        let status_slot = STATUSES
            .iter()
            .position(|known| *known == status)
            .unwrap_or(STATUSES.len() - 1);
        self.requests[route][status_slot].fetch_add(1, Relaxed);
        let seconds = elapsed.as_secs_f64();
        let bucket = BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(BUCKETS.len());
        self.duration_buckets[route][bucket].fetch_add(1, Relaxed);
        self.duration_sum_micros[route].fetch_add(
            u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
            Relaxed,
        );
    }

    pub(crate) fn response_bytes(&self, bytes: usize) {
        self.response_bytes.fetch_add(bytes as u64, Relaxed);
    }

    pub(crate) fn source_read_bytes(&self, bytes: usize) {
        self.source_read_bytes.fetch_add(bytes as u64, Relaxed);
    }

    pub(crate) fn request_shed(&self) {
        self.shed_requests.fetch_add(1, Relaxed);
    }

    pub(crate) fn segment_queue_timeout(&self) {
        self.segment_queue_timeouts.fetch_add(1, Relaxed);
    }

    pub(crate) fn stream_aborted(&self, reason: StreamAbort) {
        match reason {
            StreamAbort::Idle => &self.stream_aborts_idle,
            StreamAbort::Client => &self.stream_aborts_client,
            StreamAbort::Error => &self.stream_aborts_error,
        }
        .fetch_add(1, Relaxed);
    }

    #[allow(clippy::too_many_lines, reason = "one flat listing of every metric")]
    pub(crate) fn render(&self, dropped_log_lines: usize) -> String {
        let mut out = String::with_capacity(8 * 1024);
        let _ = writeln!(
            out,
            "# HELP vod_log_dropped_lines_total Log records dropped by the non-blocking writer.\n# TYPE vod_log_dropped_lines_total counter\nvod_log_dropped_lines_total {dropped_log_lines}"
        );

        let _ = writeln!(
            out,
            "# HELP vod_http_requests_total Requests answered, by route template and status.\n# TYPE vod_http_requests_total counter"
        );
        for (route_index, route) in self.labels().enumerate() {
            for (status_index, status) in STATUSES.iter().enumerate() {
                let count = self.requests[route_index][status_index].load(Relaxed);
                if count == 0 {
                    continue;
                }
                let status = if *status == 0 {
                    "other".to_owned()
                } else {
                    status.to_string()
                };
                let _ = writeln!(
                    out,
                    "vod_http_requests_total{{route=\"{route}\",status=\"{status}\"}} {count}"
                );
            }
        }

        let _ = writeln!(
            out,
            "# HELP vod_http_request_duration_seconds Time from request receipt to response headers.\n# TYPE vod_http_request_duration_seconds histogram"
        );
        for (route_index, route) in self.labels().enumerate() {
            let counts = &self.duration_buckets[route_index];
            let total = counts.iter().map(|count| count.load(Relaxed)).sum::<u64>();
            if total == 0 {
                continue;
            }
            let mut cumulative = 0;
            for (bound, count) in BUCKETS.iter().zip(counts) {
                cumulative += count.load(Relaxed);
                let _ = writeln!(
                    out,
                    "vod_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"{bound}\"}} {cumulative}"
                );
            }
            let sum = Duration::from_micros(self.duration_sum_micros[route_index].load(Relaxed))
                .as_secs_f64();
            let _ = writeln!(
                out,
                "vod_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"+Inf\"}} {total}\nvod_http_request_duration_seconds_sum{{route=\"{route}\"}} {sum}\nvod_http_request_duration_seconds_count{{route=\"{route}\"}} {total}"
            );
        }

        let gauges_and_counters: [(&str, &str, &str, u64); 9] = [
            (
                "vod_http_connections_open",
                "gauge",
                "TCP connections currently held open.",
                u64::try_from(self.connections_open.load(Relaxed)).unwrap_or(0),
            ),
            (
                "vod_http_connections_rejected_total",
                "counter",
                "Connections closed at accept because max_connections was reached.",
                self.connections_rejected.load(Relaxed),
            ),
            (
                "vod_http_requests_in_flight",
                "gauge",
                "Requests currently between receipt and response headers.",
                u64::try_from(self.in_flight.load(Relaxed)).unwrap_or(0),
            ),
            (
                "vod_http_response_bytes_total",
                "counter",
                "Media payload and header bytes handed to the HTTP body stream.",
                self.response_bytes.load(Relaxed),
            ),
            (
                "vod_source_read_bytes_total",
                "counter",
                "Bytes read from media sources for segment responses.",
                self.source_read_bytes.load(Relaxed),
            ),
            (
                "vod_http_requests_shed_total",
                "counter",
                "Requests rejected because max_concurrent_requests was reached.",
                self.shed_requests.load(Relaxed),
            ),
            (
                "vod_segment_queue_timeouts_total",
                "counter",
                "Segment reads that waited longer than segment_queue_timeout_ms for a job slot.",
                self.segment_queue_timeouts.load(Relaxed),
            ),
            (
                "vod_segment_stream_aborts_idle_total",
                "counter",
                "Media streams ended because the client stopped reading.",
                self.stream_aborts_idle.load(Relaxed),
            ),
            (
                "vod_segment_stream_aborts_client_total",
                "counter",
                "Media streams ended because the client disconnected.",
                self.stream_aborts_client.load(Relaxed),
            ),
        ];
        let _ = writeln!(
            out,
            "# HELP vod_resolver_requests_total Asset resolver calls by outcome.\n# TYPE vod_resolver_requests_total counter"
        );
        for (label, count) in RESOLVER_OUTCOMES.iter().zip(&self.resolver_outcomes) {
            let _ = writeln!(
                out,
                "vod_resolver_requests_total{{outcome=\"{label}\"}} {}",
                count.load(Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP vod_resolution_cache_events_total Resolution cache lookups by result.\n# TYPE vod_resolution_cache_events_total counter"
        );
        for (label, count) in CACHE_EVENTS.iter().zip(&self.cache_events) {
            let _ = writeln!(
                out,
                "vod_resolution_cache_events_total{{event=\"{label}\"}} {}",
                count.load(Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "# HELP vod_asset_loads_total Asset loads by outcome.\n# TYPE vod_asset_loads_total counter\nvod_asset_loads_total{{outcome=\"ok\"}} {}\nvod_asset_loads_total{{outcome=\"failed\"}} {}\n# HELP vod_asset_load_seconds_total Time spent loading assets.\n# TYPE vod_asset_load_seconds_total counter\nvod_asset_load_seconds_total {}\n# HELP vod_registry_coalesced_waiters_total Requests that shared another request's resolve or load.\n# TYPE vod_registry_coalesced_waiters_total counter\nvod_registry_coalesced_waiters_total {}\n# HELP vod_location_rotations_total Signed URLs replaced in place on loaded assets.\n# TYPE vod_location_rotations_total counter\nvod_location_rotations_total {}\n# HELP vod_loaded_assets Assets currently held in memory.\n# TYPE vod_loaded_assets gauge\nvod_loaded_assets {}\n# HELP vod_loaded_bytes Estimated bytes held by loaded assets.\n# TYPE vod_loaded_bytes gauge\nvod_loaded_bytes {}",
            self.loads_ok.load(Relaxed),
            self.loads_failed.load(Relaxed),
            Duration::from_micros(self.load_micros.load(Relaxed)).as_secs_f64(),
            self.coalesced_waiters.load(Relaxed),
            self.location_rotations.load(Relaxed),
            self.loaded_assets.load(Relaxed),
            self.loaded_bytes.load(Relaxed),
        );
        for (name, kind, help, value) in gauges_and_counters {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
            );
        }
        let _ = writeln!(
            out,
            "# HELP vod_segment_stream_aborts_error_total Media streams ended by a source or queue error.\n# TYPE vod_segment_stream_aborts_error_total counter\nvod_segment_stream_aborts_error_total {}",
            self.stream_aborts_error.load(Relaxed)
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_requests_by_route_and_status() {
        let metrics = Arc::new(Metrics::new(&["/health"]));
        let route = metrics.route_index(Some("/health"));
        let first = metrics.request_started();
        metrics.request_finished(route, 200, Duration::from_millis(3));
        drop(first);
        let second = metrics.request_started();
        metrics.request_finished(metrics.route_index(None), 404, Duration::from_millis(1));
        assert!(metrics.render(0).contains("vod_http_requests_in_flight 1"));
        drop(second);

        let text = metrics.render(0);

        assert!(text.contains("vod_http_requests_total{route=\"/health\",status=\"200\"} 1"));
        assert!(text.contains("vod_http_requests_total{route=\"unmatched\",status=\"404\"} 1"));
        assert!(text.contains("vod_http_requests_in_flight 0"));
        assert!(text.contains(
            "vod_http_request_duration_seconds_bucket{route=\"/health\",le=\"0.005\"} 1"
        ));
    }

    #[test]
    fn unknown_route_templates_fold_into_other() {
        let metrics = Metrics::new(&["/health"]);

        assert_eq!(metrics.route_index(Some("/surprise")), metrics.other());
        assert_eq!(metrics.route_index(Some("/health")), 0);
        assert_eq!(metrics.route_index(None), metrics.unmatched());
    }
}
