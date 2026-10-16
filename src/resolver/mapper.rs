//! The client for the mapper wire protocol (`docs/technical-design/0002-asset-map-interface.md`).
//!
//! `GET {base_url}/v1/assets/{asset_id}` returns a JSON document naming the media's location and
//! version. Everything in the answer is validated before it is trusted.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, CACHE_CONTROL, IF_NONE_MATCH, RETRY_AFTER};
use reqwest::{Client, Response, StatusCode, Url};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::time::sleep;

use super::policy::{LocationPolicy, validate_relative_path};
use super::{
    AssetLocation, ClipLocation, RenditionLocation, RenditionSource, Resolution, ResolveError,
    ResolvedAsset, SubtitleLocation,
};
use crate::clip::{ClipWindow, MAX_CLIP_MS};
use crate::config::Secret;
use crate::config::{MapperConfig, is_valid_asset_id, read_token_file};
use crate::media::{AudioOverrides, TrackSelection};
use crate::observability::request_id;

const MAX_VERSION_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 128;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(1);
/// A rendition `id` is a path segment (`video-{id}` in a URL), so it is kept short and plain.
const MAX_RENDITION_ID_BYTES: usize = 32;

/// The bearer token the mapper is called with. A token read from a file is re-read at most once
/// per interval, by whichever request finds it due, so a rotated token takes effect without a
/// restart and with no background task. A file that cannot be read keeps the last good token, so
/// the moment of a rotation (the file briefly empty or missing) does not break resolution.
#[derive(Debug)]
struct TokenSource {
    state: Mutex<TokenState>,
    file: Option<(PathBuf, Duration)>,
}

#[derive(Debug)]
struct TokenState {
    token: Secret,
    checked: Instant,
}

impl TokenSource {
    fn new(settings: &MapperConfig) -> Option<Self> {
        let token = settings.bearer_token.clone()?;
        let file = settings.bearer_token_file.clone().and_then(|path| {
            (settings.bearer_token_reload_ms > 0)
                .then(|| (path, Duration::from_millis(settings.bearer_token_reload_ms)))
        });
        Some(Self {
            state: Mutex::new(TokenState {
                token,
                checked: Instant::now(),
            }),
            file,
        })
    }

    fn current(&self) -> Secret {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((path, interval)) = &self.file
            && state.checked.elapsed() >= *interval
        {
            state.checked = Instant::now();
            match read_token_file(path) {
                Ok(token) if token.expose() != state.token.expose() => {
                    tracing::info!(path = %path.display(), "mapper bearer token reloaded");
                    state.token = token;
                }
                Ok(_) => {}
                Err(reason) => tracing::warn!(
                    path = %path.display(),
                    %reason,
                    "mapper bearer token file not reloaded; keeping the previous token"
                ),
            }
        }
        state.token.clone()
    }
}

#[derive(Debug)]
pub(crate) struct HttpResolver {
    client: Client,
    base_url: String,
    token: Option<TokenSource>,
    settings: MapperConfig,
    policy: LocationPolicy,
    /// `limits.max_clips`: an answer listing more is malformed.
    max_clips: usize,
    /// `limits.max_sequence_files`: renditions times clips may not exceed it (TDD 0016).
    max_sequence_files: usize,
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
    /// Several files, or windows of them, played back to back, instead of `location`. See
    /// `docs/technical-design/0008-clipping-and-concatenation.md`.
    #[serde(default)]
    clips: Vec<WireClip>,
    #[serde(default)]
    subtitles: Vec<WireSubtitle>,
    /// Content keys and DRM systems (TDD 0009). Validated before the answer is used.
    #[serde(default)]
    encryption: Option<crate::cenc::WireEncryption>,
    /// Whole-segment HLS `AES-128` for a single file (TDD 0012).
    #[serde(default)]
    hls_aes128: Option<crate::cenc::WireAes128>,
    /// Keys for a source that is already encrypted (TDD 0014). Validated before use.
    #[serde(default)]
    source_encryption: Option<crate::cenc::WireSourceEncryption>,
    /// Which tracks of a single file to serve (TDD 0015), such as `v1-a2`.
    #[serde(default)]
    tracks: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireClip {
    location: WireLocation,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    /// Overrides the answer's own `encryption` for this clip only (TDD 0009, "Different keys per
    /// clip"). Absent means "use the answer's `encryption`", which may itself be absent (clear).
    #[serde(default)]
    encryption: Option<crate::cenc::WireEncryption>,
    /// Which tracks of the file to serve (TDD 0015).
    #[serde(default)]
    tracks: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireRendition {
    id: String,
    /// One file; mutually exclusive with `clips` (TDD 0016).
    #[serde(default)]
    location: Option<WireLocation>,
    /// A sequence of clips instead of one file (TDD 0016).
    #[serde(default)]
    clips: Vec<WireClip>,
    /// Which tracks of the file to serve (TDD 0015).
    #[serde(default)]
    tracks: Option<String>,
    /// The language, label and default of the audio this rendition brings, in place of its
    /// file's (TDD 0015).
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    default: bool,
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
        max_clips: usize,
        max_sequence_files: usize,
    ) -> crate::error::Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(settings.connect_timeout_ms))
            .timeout(Duration::from_millis(settings.request_timeout_ms))
            .user_agent(concat!("vodnik/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                crate::error::Error::Configuration(format!("mapper client: {error}"))
            })?;
        Ok(Self {
            client,
            base_url: settings.base_url.clone(),
            token: TokenSource::new(settings),
            settings: settings.clone(),
            policy,
            max_clips,
            max_sequence_files,
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

    /// `GET {base}/v1/assets`: the asset IDs the mapper chooses to list, for status reporting
    /// only (see `docs/mapper-api.md#list-assets`). The endpoint is optional, so any failure,
    /// including the `404` of a mapper that does not implement it, is `None` rather than an
    /// error. IDs a request could not name are dropped; the rest are sorted, deduplicated, and
    /// capped at `max`.
    pub(crate) async fn list(&self, max: usize) -> Option<Vec<String>> {
        #[derive(Deserialize)]
        struct Listing {
            assets: Vec<String>,
        }
        let response = self
            .authorized(self.client.get(format!("{}/v1/assets", self.base_url)))
            .header(ACCEPT, "application/json")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = self.read_body(response).await.ok()?;
        let listing: Listing = serde_json::from_slice(&body).ok()?;
        let mut ids = listing
            .assets
            .into_iter()
            .filter(|id| is_valid_asset_id(id))
            .collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        ids.truncate(max);
        Some(ids)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.header(
                AUTHORIZATION,
                format!("Bearer {}", token.current().expose()),
            ),
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
                let wire = parse_answer(&body)?;
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
            let tracks = entry
                .tracks
                .as_deref()
                .map(TrackSelection::parse)
                .transpose()
                .map_err(|message| {
                    ResolveError::Rejected(format!("rendition `{}`: {message}", entry.id))
                })?;
            let audio = Self::interpret_audio_overrides(entry)?;
            let source = match (&entry.location, entry.clips.is_empty()) {
                (Some(location), true) => {
                    RenditionSource::Location(self.interpret_location(location)?)
                }
                (None, false) => {
                    if entry.tracks.is_some() {
                        return reject(format!(
                            "rendition `{}`: tracks belongs on each clip of a rendition that has clips",
                            entry.id
                        ));
                    }
                    RenditionSource::Clips(self.interpret_clips(&entry.clips).map_err(|error| {
                        match error {
                            ResolveError::Rejected(message) => ResolveError::Rejected(format!(
                                "rendition `{}`: {message}",
                                entry.id
                            )),
                            other => other,
                        }
                    })?)
                }
                _ => {
                    return reject(format!(
                        "rendition `{}` must set exactly one of location or clips",
                        entry.id
                    ));
                }
            };
            if audio.as_ref().is_some_and(|audio| audio.default)
                && renditions.iter().any(|known: &RenditionLocation| {
                    known.audio.as_ref().is_some_and(|audio| audio.default)
                })
            {
                return reject("more than one rendition is marked default".to_owned());
            }
            renditions.push(RenditionLocation {
                id: entry.id.clone(),
                source,
                tracks,
                audio,
            });
        }
        self.check_rendition_clips(&renditions)?;
        Ok(renditions)
    }

    /// Renditions that are sequences (TDD 0016): none may sit beside one that is a single file,
    /// all have the same number of clips, and together they name at most
    /// `limits.max_sequence_files` files.
    fn check_rendition_clips(&self, renditions: &[RenditionLocation]) -> Result<(), ResolveError> {
        let counts = renditions
            .iter()
            .filter_map(|rendition| match &rendition.source {
                RenditionSource::Clips(clips) => Some((rendition.id.as_str(), clips.len())),
                RenditionSource::Location(_) => None,
            })
            .collect::<Vec<_>>();
        let Some(&(first_id, first_count)) = counts.first() else {
            return Ok(());
        };
        if counts.len() != renditions.len() {
            return Err(ResolveError::Rejected(
                "renditions that have clips cannot be mixed with renditions that have a location"
                    .to_owned(),
            ));
        }
        if let Some(&(id, count)) = counts.iter().find(|&&(_, count)| count != first_count) {
            return Err(ResolveError::Rejected(format!(
                "rendition `{id}` has {count} clips but `{first_id}` has {first_count}"
            )));
        }
        let files = counts.len().saturating_mul(first_count);
        if files > self.max_sequence_files {
            return Err(ResolveError::Rejected(format!(
                "mapper listed {files} files across renditions and clips, more than \
                 limits.max_sequence_files ({})",
                self.max_sequence_files
            )));
        }
        Ok(())
    }

    /// A rendition's `language`, `label` and `default`, checked; `None` when it sets none.
    fn interpret_audio_overrides(
        entry: &WireRendition,
    ) -> Result<Option<AudioOverrides>, ResolveError> {
        let reject = |what: &str| {
            Err(ResolveError::Rejected(format!(
                "rendition `{}`: {what}",
                entry.id
            )))
        };
        if let Some(language) = &entry.language
            && !(language.len() == 3 && language.bytes().all(|byte| byte.is_ascii_lowercase()))
        {
            return reject("language must be a three-letter ISO 639 code in lower case");
        }
        if let Some(label) = &entry.label
            && (label.is_empty()
                || label.len() > MAX_LABEL_BYTES
                || label.chars().any(|c| {
                    // The label is written into XML, which cannot hold these two characters.
                    c.is_control() || matches!(c, '\u{fffe}' | '\u{ffff}')
                }))
        {
            return reject(
                "label is empty, overlong, or has a character a playlist or manifest cannot hold",
            );
        }
        let overrides = AudioOverrides {
            language: entry.language.clone(),
            label: entry.label.clone(),
            default: entry.default,
        };
        Ok((!overrides.is_empty()).then_some(overrides))
    }

    /// The `tracks` of an answer, which belongs to a single file: renditions and clips carry their
    /// own (TDD 0015).
    fn interpret_tracks(
        tracks: Option<&str>,
        single_file: bool,
    ) -> Result<Option<TrackSelection>, ResolveError> {
        let tracks = tracks
            .map(TrackSelection::parse)
            .transpose()
            .map_err(ResolveError::Rejected)?;
        if tracks.is_some() && !single_file {
            return Err(ResolveError::Rejected(
                "tracks on the answer is for a single file; put it on each rendition or clip"
                    .to_owned(),
            ));
        }
        Ok(tracks)
    }

    /// A sequence of clips instead of one `location`. Windows are checked here; whether a window
    /// holds any media waits until its file is parsed (`clip::trim`).
    fn interpret_clips(&self, wire: &[WireClip]) -> Result<Vec<ClipLocation>, ResolveError> {
        if wire.len() > self.max_clips {
            return Err(ResolveError::Rejected(format!(
                "mapper listed {} clips, more than limits.max_clips ({})",
                wire.len(),
                self.max_clips
            )));
        }
        wire.iter()
            .enumerate()
            .map(|(position, clip)| {
                let from_ms = clip.from_ms.unwrap_or(0);
                if from_ms > MAX_CLIP_MS || clip.to_ms.is_some_and(|to_ms| to_ms > MAX_CLIP_MS) {
                    return Err(ResolveError::Rejected(format!(
                        "clip {position}: times must be at most {MAX_CLIP_MS} ms"
                    )));
                }
                if clip.to_ms.is_some_and(|to_ms| to_ms <= from_ms) {
                    return Err(ResolveError::Rejected(format!(
                        "clip {position}: to_ms must be later than from_ms"
                    )));
                }
                Ok(ClipLocation {
                    location: self.interpret_location(&clip.location)?,
                    window: ClipWindow {
                        from_ms,
                        to_ms: clip.to_ms,
                    },
                    encryption: Self::interpret_clip_encryption(clip.encryption.as_ref()).map_err(
                        |message| ResolveError::Rejected(format!("clip {position}: {message}")),
                    )?,
                    tracks: clip
                        .tracks
                        .as_deref()
                        .map(TrackSelection::parse)
                        .transpose()
                        .map_err(|message| {
                            ResolveError::Rejected(format!("clip {position}: {message}"))
                        })?,
                })
            })
            .collect()
    }

    /// Validates an `encryption` object, from either the answer or one clip. `None` stays `None`.
    fn interpret_encryption(
        wire: Option<&crate::cenc::WireEncryption>,
    ) -> Result<Option<Arc<crate::cenc::Encryption>>, String> {
        wire.map(|encryption| encryption.validate().map(Arc::new))
            .transpose()
    }

    /// A clip's own `encryption`: a single key set, never a timeline (TDD 0013).
    fn interpret_clip_encryption(
        wire: Option<&crate::cenc::WireEncryption>,
    ) -> Result<Option<Arc<crate::cenc::Encryption>>, String> {
        let encryption = Self::interpret_encryption(wire)?;
        if encryption.as_ref().is_some_and(|e| e.is_rotating()) {
            return Err("a clip's encryption cannot have periods".to_owned());
        }
        Ok(encryption)
    }

    /// What stage 1 of TDD 0016 does not serve with renditions that have clips: encryption of any
    /// kind, and subtitles. Each is refused with a message that points to the design.
    fn refuse_unsupported_with_rendition_clips(
        wire: &Wire,
        renditions: &[RenditionLocation],
    ) -> Result<(), ResolveError> {
        if !renditions
            .iter()
            .any(|rendition| matches!(rendition.source, RenditionSource::Clips(_)))
        {
            return Ok(());
        }
        let refuse = |what: &str| {
            Err(ResolveError::Rejected(format!(
                "{what} is not supported yet with renditions that have clips (TDD 0016)"
            )))
        };
        if !wire.subtitles.is_empty() {
            return refuse("subtitles");
        }
        if wire.encryption.is_some() {
            return refuse("encryption");
        }
        if wire.hls_aes128.is_some() {
            return refuse("hls_aes128");
        }
        if wire.source_encryption.is_some() {
            return refuse("source_encryption");
        }
        let encrypted_clip = renditions
            .iter()
            .find_map(|rendition| match &rendition.source {
                RenditionSource::Clips(clips) => clips
                    .iter()
                    .position(|clip| clip.encryption.is_some())
                    .map(|position| (rendition.id.as_str(), position)),
                RenditionSource::Location(_) => None,
            });
        if let Some((id, position)) = encrypted_clip {
            return Err(ResolveError::Rejected(format!(
                "rendition `{id}`: clip {position}: encryption is not supported yet with \
                 renditions that have clips (TDD 0016)"
            )));
        }
        Ok(())
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
        let alternatives = usize::from(wire.location.is_some())
            + usize::from(!wire.renditions.is_empty())
            + usize::from(!wire.clips.is_empty());
        if alternatives != 1 {
            return reject("mapper answer must set exactly one of location, renditions, or clips");
        }
        let location = wire
            .location
            .as_ref()
            .map(|location| self.interpret_location(location))
            .transpose()?;
        let renditions = self.interpret_renditions(&wire.renditions)?;
        let clips = self.interpret_clips(&wire.clips)?;
        if !clips.is_empty() && !wire.subtitles.is_empty() {
            // A clip starts on a keyframe, so the mapper cannot know where the served timeline
            // begins; cues written for it would drift by up to a GOP (TDD 0008, "Deferred").
            return reject("subtitles cannot be combined with clips");
        }
        Self::refuse_unsupported_with_rendition_clips(&wire, &renditions)?;
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
        let encryption =
            Self::interpret_encryption(wire.encryption.as_ref()).map_err(ResolveError::Rejected)?;
        // Stage 2 of TDD 0013: a single file or every rendition of one; not yet clips.
        if encryption.as_ref().is_some_and(|e| e.is_rotating()) && !clips.is_empty() {
            return Err(ResolveError::Rejected(
                "encryption periods cannot be combined with clips".to_owned(),
            ));
        }
        let hls_aes128 = wire
            .hls_aes128
            .as_ref()
            .map(|aes128| aes128.validate().map(Arc::new))
            .transpose()
            .map_err(ResolveError::Rejected)?;
        if hls_aes128.is_some() {
            // One key for the whole asset: every rendition and every clip is encrypted under it.
            if encryption.is_some() {
                return Err(ResolveError::Rejected(
                    "hls_aes128 and encryption cannot both be set".to_owned(),
                ));
            }
            if clips.iter().any(|clip| clip.encryption.is_some()) {
                return Err(ResolveError::Rejected(
                    "hls_aes128 and a clip's encryption cannot both be set".to_owned(),
                ));
            }
        }
        let source_encryption = wire
            .source_encryption
            .as_ref()
            .map(|source| source.validate().map(Arc::new))
            .transpose()
            .map_err(ResolveError::Rejected)?;
        // Stage 1 of TDD 0014 reads one encrypted file; renditions and clips come later.
        if source_encryption.is_some() && location.is_none() {
            return Err(ResolveError::Rejected(
                "source_encryption is supported for a single file only".to_owned(),
            ));
        }
        let tracks = Self::interpret_tracks(wire.tracks.as_deref(), location.is_some())?;
        Ok(ResolvedAsset {
            location,
            renditions,
            clips,
            subtitles,
            version: wire.version,
            valid_until,
            hard_expiry,
            encryption,
            hls_aes128,
            source_encryption,
            tracks,
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

/// Parses a `200` body. Only the location of a failure is reported: serde's type-mismatch
/// messages can echo a value from the body, and the body may hold content keys.
fn parse_answer(body: &[u8]) -> Result<Wire, ResolveError> {
    serde_json::from_slice(body).map_err(|error| {
        ResolveError::Rejected(format!(
            "mapper answer is not valid JSON of the expected shape (line {}, column {})",
            error.line(),
            error.column()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mistyped_answer_reports_its_position_but_never_a_value() {
        let key = "00112233445566778899aabbccddeeff";
        let body = format!(
            r#"{{"asset_id":"a","version":"v","encryption":{{"scheme":"cbcs","keys":"{key}"}}}}"#
        );

        let Err(ResolveError::Rejected(message)) = parse_answer(body.as_bytes()) else {
            panic!("a string where an array belongs must be rejected");
        };

        assert!(!message.contains(key), "{message}");
        assert!(message.contains("line"), "{message}");
    }

    use serde_json::{Value, json};

    use crate::config::RemoteMediaConfig;

    const KEY_ID: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff";

    fn resolver(max_clips: usize, max_sequence_files: usize) -> HttpResolver {
        let settings = MapperConfig {
            base_url: "https://mapper.invalid".to_owned(),
            bearer_token: None,
            bearer_token_file: None,
            bearer_token_reload_ms: 0,
            connect_timeout_ms: 1000,
            request_timeout_ms: 1000,
            max_retries: 0,
            max_response_bytes: 65536,
            default_ttl_ms: 300_000,
            min_ttl_ms: 1000,
            max_ttl_ms: 3_600_000,
            negative_ttl_ms: 1000,
            error_ttl_ms: 1000,
            stale_if_error_ms: 0,
            refresh_margin_ms: 30_000,
            readiness_probe_interval_ms: 0,
        };
        HttpResolver::new(
            &settings,
            LocationPolicy::new(&RemoteMediaConfig::default()),
            max_clips,
            max_sequence_files,
        )
        .expect("the test settings are valid")
    }

    fn clip(path: &str) -> Value {
        json!({ "location": { "type": "file", "path": path } })
    }

    fn clips_rendition(id: &str, paths: &[&str]) -> Value {
        json!({ "id": id, "clips": paths.iter().map(|path| clip(path)).collect::<Vec<_>>() })
    }

    fn answer(renditions: &[Value]) -> Value {
        json!({ "asset_id": "movie", "version": "v1", "renditions": renditions })
    }

    fn interpret(resolver: &HttpResolver, answer: &Value) -> Result<ResolvedAsset, ResolveError> {
        let wire = parse_answer(answer.to_string().as_bytes())?;
        resolver.interpret("movie", wire, None)
    }

    fn rejected(result: Result<ResolvedAsset, ResolveError>) -> String {
        match result {
            Err(ResolveError::Rejected(message)) => message,
            other => panic!("expected the answer to be rejected, got {other:?}"),
        }
    }

    #[test]
    fn a_rendition_may_have_clips_instead_of_a_location() {
        let mut english = clips_rendition("en", &["ad-en.m4a", "movie-en.m4a"]);
        english["language"] = json!("eng");
        english["label"] = json!("English");
        english["default"] = json!(true);
        let answer = answer(&[clips_rendition("720p", &["ad.mp4", "movie.mp4"]), english]);

        let resolved = interpret(&resolver(64, 256), &answer).unwrap();

        assert_eq!(resolved.renditions.len(), 2);
        for rendition in &resolved.renditions {
            let RenditionSource::Clips(clips) = &rendition.source else {
                panic!("rendition `{}` should have clips", rendition.id);
            };
            assert_eq!(clips.len(), 2);
        }
        let audio = resolved.renditions[1].audio.as_ref().unwrap();
        assert_eq!(audio.language.as_deref(), Some("eng"));
        assert_eq!(audio.label.as_deref(), Some("English"));
        assert!(audio.default);
    }

    #[test]
    fn a_rendition_with_both_or_neither_of_location_and_clips_is_rejected() {
        let mut both = clips_rendition("a", &["x.mp4"]);
        both["location"] = json!({ "type": "file", "path": "x.mp4" });
        let message = rejected(interpret(&resolver(64, 256), &answer(&[both])));
        assert!(
            message.contains("rendition `a` must set exactly one of location or clips"),
            "{message}"
        );

        let message = rejected(interpret(
            &resolver(64, 256),
            &answer(&[json!({ "id": "a" })]),
        ));
        assert!(
            message.contains("rendition `a` must set exactly one of location or clips"),
            "{message}"
        );
    }

    #[test]
    fn renditions_with_clips_and_renditions_with_a_location_cannot_mix() {
        let located = json!({ "id": "b", "location": { "type": "file", "path": "b.mp4" } });
        let message = rejected(interpret(
            &resolver(64, 256),
            &answer(&[clips_rendition("a", &["x.mp4"]), located]),
        ));
        assert!(message.contains("cannot be mixed"), "{message}");
    }

    #[test]
    fn renditions_with_clips_must_have_equal_clip_counts() {
        let message = rejected(interpret(
            &resolver(64, 256),
            &answer(&[
                clips_rendition("a", &["x.mp4", "y.mp4"]),
                clips_rendition("b", &["z.mp4"]),
            ]),
        ));
        assert!(
            message.contains("rendition `b` has 1 clips but `a` has 2"),
            "{message}"
        );
    }

    #[test]
    fn tracks_on_a_rendition_with_clips_is_rejected() {
        let mut rendition = clips_rendition("a", &["x.mp4"]);
        rendition["tracks"] = json!("v1");
        let message = rejected(interpret(&resolver(64, 256), &answer(&[rendition])));
        assert!(message.contains("tracks belongs on each clip"), "{message}");
    }

    #[test]
    fn more_files_than_max_sequence_files_is_rejected_before_any_fetch() {
        let renditions: Vec<Value> = ["a", "b", "c"]
            .iter()
            .map(|id| clips_rendition(id, &["x.mp4", "y.mp4"]))
            .collect();
        let message = rejected(interpret(&resolver(64, 4), &answer(&renditions)));
        assert!(
            message.contains(
                "mapper listed 6 files across renditions and clips, more than \
                 limits.max_sequence_files (4)"
            ),
            "{message}"
        );
    }

    #[test]
    fn clips_per_rendition_obey_max_clips() {
        let message = rejected(interpret(
            &resolver(1, 256),
            &answer(&[clips_rendition("a", &["x.mp4", "y.mp4"])]),
        ));
        assert!(message.contains("rendition `a`: "), "{message}");
        assert!(message.contains("limits.max_clips"), "{message}");
    }

    #[test]
    fn stage_one_refuses_encryption_and_subtitles_with_rendition_clips() {
        let base = || answer(&[clips_rendition("a", &["x.mp4"])]);
        let encryption = json!({
            "scheme": "cbcs",
            "keys": [{ "key_id": KEY_ID, "key": KEY }],
            "systems": [{ "system_id": "e2719d58-a985-b3c9-781a-b030af78d30e", "license_url": "https://l.example.net/ck" }],
        });
        let mut cases = Vec::new();
        let mut answer_encryption = base();
        answer_encryption["encryption"] = encryption.clone();
        cases.push(("encryption", answer_encryption));
        let mut aes128 = base();
        aes128["hls_aes128"] = json!({ "key": KEY, "key_uri": "https://keys.example.com/k" });
        cases.push(("hls_aes128", aes128));
        let mut source = base();
        source["source_encryption"] = json!({ "keys": [{ "key_id": KEY_ID, "key": KEY }] });
        cases.push(("source_encryption", source));
        let mut subtitles = base();
        subtitles["subtitles"] = json!([{
            "language": "en",
            "label": "EN",
            "location": { "type": "file", "path": "s.vtt" },
        }]);
        cases.push(("subtitles", subtitles));
        let mut clip_encryption = base();
        clip_encryption["renditions"][0]["clips"][0]["encryption"] = encryption;
        cases.push(("a clip's encryption", clip_encryption));

        for (name, answer) in cases {
            let message = rejected(interpret(&resolver(64, 256), &answer));
            assert!(message.contains("TDD 0016"), "{name}: {message}");
        }
    }

    #[test]
    fn a_clip_error_names_its_rendition() {
        let mut second = clips_rendition("b", &["x.mp4", "y.mp4"]);
        second["clips"][1]["from_ms"] = json!(500);
        second["clips"][1]["to_ms"] = json!(100);
        let message = rejected(interpret(
            &resolver(64, 256),
            &answer(&[clips_rendition("a", &["x.mp4", "y.mp4"]), second]),
        ));
        assert!(message.contains("rendition `b`: clip 1"), "{message}");
    }
}
