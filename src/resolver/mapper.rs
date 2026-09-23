//! The client for the mapper wire protocol (`docs/technical-design/0002-asset-map-interface.md`).
//!
//! `GET {base_url}/v1/assets/{asset_id}` returns a JSON document naming the media's location and
//! version. Everything in the answer is validated before it is trusted.

use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, IF_NONE_MATCH, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::sleep;

use super::policy::{LocationPolicy, validate_relative_path};
use super::{
    AssetLocation, RenditionLocation, Resolution, ResolveError, ResolvedAsset, SubtitleLocation,
};
use crate::config::MapperConfig;
use crate::config::Secret;
use crate::observability::request_id;

const MAX_VERSION_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(1);
/// A rendition `id` is a path segment (`video-{id}` in a URL), so it is kept short and plain.
const MAX_RENDITION_ID_BYTES: usize = 32;

#[derive(Debug)]
pub(crate) struct HttpResolver {
    client: Client,
    base_url: String,
    token: Option<Secret>,
    settings: MapperConfig,
    policy: LocationPolicy,
}

/// The JSON body of a `200` answer. Unknown fields are ignored so the contract can grow.
#[derive(Debug, Deserialize)]
struct Wire {
    asset_id: String,
    version: String,
    ttl_seconds: Option<u64>,
    expires_at: Option<String>,
    #[serde(default)]
    location: Option<WireLocation>,
    /// Several files served as one adaptive asset, instead of `location`. See
    /// `docs/technical-design/0006-trick-play-subtitles-and-renditions.md`.
    #[serde(default)]
    renditions: Vec<WireRendition>,
    #[serde(default)]
    subtitles: Vec<WireSubtitle>,
}

#[derive(Debug, Deserialize)]
struct WireRendition {
    id: String,
    location: WireLocation,
}

#[derive(Debug, Deserialize)]
struct WireSubtitle {
    language: String,
    label: Option<String>,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    forced: bool,
    location: WireLocation,
}

/// `serde` rejects an unknown `type`, which is how unknown location kinds are refused.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireLocation {
    File { path: String },
    Http { url: String },
}

/// One attempt's failure, with the delay the mapper asked for.
struct Failure {
    error: ResolveError,
    retry_after: Option<Duration>,
}

impl From<ResolveError> for Failure {
    fn from(error: ResolveError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

impl HttpResolver {
    pub(crate) fn new(
        settings: &MapperConfig,
        policy: LocationPolicy,
    ) -> crate::error::Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(settings.connect_timeout_ms))
            .timeout(Duration::from_millis(settings.request_timeout_ms))
            .user_agent(concat!("segmentor/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                crate::error::Error::Configuration(format!("mapper client: {error}"))
            })?;
        Ok(Self {
            client,
            base_url: settings.base_url.clone(),
            token: settings.bearer_token.clone(),
            settings: settings.clone(),
            policy,
        })
    }

    pub(crate) async fn resolve(
        &self,
        asset_id: &str,
        known_version: Option<&str>,
    ) -> Result<Resolution, ResolveError> {
        let request_id = request_id::generate();
        let mut attempt = 0;
        loop {
            match self.attempt(asset_id, known_version, &request_id).await {
                Err(failure)
                    if matches!(failure.error, ResolveError::Unavailable(_))
                        && attempt < self.settings.max_retries =>
                {
                    attempt += 1;
                    let backoff = Duration::from_millis(50 * u64::from(attempt));
                    let delay = failure.retry_after.unwrap_or(backoff).min(MAX_RETRY_AFTER);
                    tracing::warn!(
                        event = "resolve_retry",
                        asset.id = asset_id,
                        attempt,
                        request_id,
                    );
                    sleep(delay).await;
                }
                Err(failure) => return Err(failure.error),
                Ok(resolution) => return Ok(resolution),
            }
        }
    }

    /// The mapper's own base URL, for status reporting.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET {base}/v1/health`; any `2xx` counts as healthy.
    pub(crate) async fn healthy(&self) -> bool {
        self.authorized(self.client.get(format!("{}/v1/health", self.base_url)))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.header(AUTHORIZATION, format!("Bearer {}", token.expose())),
            None => request,
        }
    }

    async fn attempt(
        &self,
        asset_id: &str,
        known_version: Option<&str>,
        request_id: &str,
    ) -> Result<Resolution, Failure> {
        let mut request = self
            .authorized(
                self.client
                    .get(format!("{}/v1/assets/{asset_id}", self.base_url)),
            )
            .header(ACCEPT, "application/json")
            .header("x-request-id", request_id);
        if let Some(version) = known_version {
            request = request.header(IF_NONE_MATCH, format!("\"{version}\""));
        }
        let response = request.send().await.map_err(|error| {
            ResolveError::Unavailable(format!("mapper unreachable: {}", error.without_url()))
        })?;

        let status = response.status();
        match status {
            StatusCode::OK => {
                let max_age = max_age(&response);
                let body = self.read_body(response).await?;
                let wire: Wire = serde_json::from_slice(&body).map_err(|error| {
                    ResolveError::Rejected(format!("mapper answer is not valid: {error}"))
                })?;
                Ok(Resolution::Resolved(
                    self.interpret(asset_id, wire, max_age)?,
                ))
            }
            StatusCode::NOT_MODIFIED if known_version.is_some() => Ok(Resolution::Unchanged {
                valid_until: Instant::now() + self.ttl(None, max_age(&response)),
            }),
            StatusCode::NOT_FOUND | StatusCode::GONE => Err(ResolveError::NotFound.into()),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                tracing::error!(
                    event = "resolve_unauthorized",
                    http.status = status.as_u16()
                );
                Err(
                    ResolveError::Rejected("mapper refused this service's credentials".to_owned())
                        .into(),
                )
            }
            StatusCode::TOO_MANY_REQUESTS => Err(Failure {
                retry_after: response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs),
                error: ResolveError::Unavailable("mapper is shedding load".to_owned()),
            }),
            status if status.is_server_error() => {
                Err(ResolveError::Unavailable(format!("mapper answered {status}")).into())
            }
            status => {
                Err(ResolveError::Rejected(format!("unexpected mapper status {status}")).into())
            }
        }
    }

    /// Reads the body, failing as soon as it exceeds `max_response_bytes`.
    async fn read_body(&self, mut response: Response) -> Result<Vec<u8>, ResolveError> {
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            ResolveError::Unavailable(format!("mapper body failed: {}", error.without_url()))
        })? {
            if body.len() + chunk.len() > self.settings.max_response_bytes {
                return Err(ResolveError::Rejected(
                    "mapper answer exceeds max_response_bytes".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// The reuse window: an explicit TTL, else the `Cache-Control` max-age, else the default,
    /// clamped to the configured bounds.
    fn ttl(&self, explicit_seconds: Option<u64>, max_age_seconds: Option<u64>) -> Duration {
        let requested = explicit_seconds
            .or(max_age_seconds)
            .map_or(self.settings.default_ttl_ms, |seconds| {
                seconds.saturating_mul(1000)
            });
        Duration::from_millis(requested.clamp(self.settings.min_ttl_ms, self.settings.max_ttl_ms))
    }

    /// A location, checked against the path rules or the remote-media policy.
    fn interpret_location(&self, wire: &WireLocation) -> Result<AssetLocation, ResolveError> {
        match wire {
            WireLocation::File { path } => Ok(AssetLocation::File(
                validate_relative_path(path).map_err(ResolveError::Rejected)?,
            )),
            WireLocation::Http { url } => {
                let url = Url::parse(url)
                    .map_err(|_| ResolveError::Rejected("location URL is not valid".to_owned()))?;
                self.policy
                    .check_url(&url)
                    .map_err(ResolveError::Rejected)?;
                Ok(AssetLocation::Http(url))
            }
        }
    }

    fn interpret_subtitles(
        &self,
        wire: &[WireSubtitle],
    ) -> Result<Vec<SubtitleLocation>, ResolveError> {
        let reject = |message: String| Err(ResolveError::Rejected(message));
        let mut subtitles: Vec<SubtitleLocation> = Vec::with_capacity(wire.len());
        for entry in wire {
            if !is_language_tag(&entry.language) {
                return reject(format!(
                    "subtitle language `{}` is not a BCP 47 tag of letters, digits and hyphens",
                    entry.language.escape_default()
                ));
            }
            if subtitles
                .iter()
                .any(|known| known.language.eq_ignore_ascii_case(&entry.language))
            {
                return reject(format!(
                    "subtitle language `{}` is listed twice",
                    entry.language
                ));
            }
            let label = entry
                .label
                .clone()
                .unwrap_or_else(|| entry.language.clone());
            if label.is_empty()
                || label.len() > MAX_LABEL_BYTES
                || label.chars().any(char::is_control)
            {
                return reject(format!(
                    "subtitle `{}` has an empty, overlong, or control-character label",
                    entry.language
                ));
            }
            subtitles.push(SubtitleLocation {
                language: entry.language.clone(),
                label,
                default: entry.default,
                forced: entry.forced,
                location: self.interpret_location(&entry.location)?,
            });
        }
        if subtitles.iter().filter(|subtitle| subtitle.default).count() > 1 {
            return reject("more than one subtitle is marked default".to_owned());
        }
        Ok(subtitles)
    }

    /// Several files served as one adaptive asset, instead of a single `location`. Classifying
    /// which are video and which are audio-only waits until each is loaded and its tracks are
    /// known (`composite::assemble`); here only the wire shape is checked: an `id` for each, and
    /// no two alike.
    fn interpret_renditions(
        &self,
        wire: &[WireRendition],
    ) -> Result<Vec<RenditionLocation>, ResolveError> {
        let reject = |message: String| Err(ResolveError::Rejected(message));
        let mut renditions = Vec::with_capacity(wire.len());
        for entry in wire {
            if !is_rendition_id(&entry.id) {
                return reject(format!(
                    "rendition id `{}` must be 1 to {MAX_RENDITION_ID_BYTES} URL-safe characters \
                     (letters, digits, hyphens)",
                    entry.id.escape_default()
                ));
            }
            if renditions
                .iter()
                .any(|known: &RenditionLocation| known.id == entry.id)
            {
                return reject(format!("rendition id `{}` is listed twice", entry.id));
            }
            renditions.push(RenditionLocation {
                id: entry.id.clone(),
                location: self.interpret_location(&entry.location)?,
            });
        }
        Ok(renditions)
    }

    /// Validates a `200` answer against the request and the location policy.
    fn interpret(
        &self,
        asset_id: &str,
        wire: Wire,
        max_age_seconds: Option<u64>,
    ) -> Result<ResolvedAsset, ResolveError> {
        let reject = |message: &str| Err(ResolveError::Rejected(message.to_owned()));
        if wire.asset_id != asset_id {
            return reject("mapper answered for a different asset ID");
        }
        if wire.version.is_empty()
            || wire.version.len() > MAX_VERSION_BYTES
            || !wire
                .version
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return reject("mapper version must be 1 to 256 visible ASCII characters");
        }
        let (location, renditions) = match (&wire.location, wire.renditions.is_empty()) {
            (Some(location), true) => (Some(self.interpret_location(location)?), Vec::new()),
            (None, false) => (None, self.interpret_renditions(&wire.renditions)?),
            (Some(_), false) => {
                return reject("mapper answer must set only one of location or renditions");
            }
            (None, true) => {
                return reject("mapper answer must set one of location or renditions");
            }
        };
        let subtitles = self.interpret_subtitles(&wire.subtitles)?;

        let now = Instant::now();
        let mut valid_until = now + self.ttl(wire.ttl_seconds, max_age_seconds);
        let mut hard_expiry = None;
        if let Some(expires_at) = wire.expires_at {
            let deadline = OffsetDateTime::parse(&expires_at, &Rfc3339)
                .map_err(|_| ResolveError::Rejected("expires_at is not RFC 3339".to_owned()))?;
            let remaining = deadline - OffsetDateTime::now_utc();
            let remaining = Duration::try_from(remaining)
                .ok()
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| ResolveError::Rejected("location has already expired".to_owned()))?;
            hard_expiry = Some(now + remaining);
            // Refresh ahead of the deadline, but not before half the lifetime has passed.
            let margin = Duration::from_millis(self.settings.refresh_margin_ms);
            let refresh_after = remaining.saturating_sub(margin).max(remaining / 2);
            valid_until = valid_until.min(now + refresh_after);
        }
        Ok(ResolvedAsset {
            location,
            renditions,
            subtitles,
            version: wire.version,
            valid_until,
            hard_expiry,
        })
    }
}

/// `Cache-Control: max-age=N`, if present.
fn max_age(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(CACHE_CONTROL)?
        .to_str()
        .ok()?
        .split(',')
        .find_map(|directive| directive.trim().strip_prefix("max-age=")?.parse().ok())
}

/// A language tag as it can appear in a URL path: letters, digits, and single hyphens, starting
/// with a letter, at most 35 characters.
fn is_language_tag(tag: &str) -> bool {
    tag.len() <= 35
        && tag
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        && tag.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
}

/// A rendition id as it appears in a URL path segment (`video-{id}`): letters, digits, and
/// hyphens, starting with a letter or digit, bounded in length. Unlike a language tag, a hyphen
/// may repeat or trail, since ids are opaque labels an operator picks, not structured tags.
fn is_rendition_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RENDITION_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
