//! A media source backed by ranged HTTP requests.
//!
//! Every read is a `Range` request conditioned on the validator captured when the source was
//! opened, so an object that changes underneath us fails the read instead of mixing bytes from
//! two versions. The client never follows redirects and resolves names through a filter that
//! drops private and loopback addresses unless the operator allows them.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode, Url};
use tokio::sync::Semaphore;
use tokio::time::{Duration, sleep};

use super::{ByteRange, Origin, SourceIdentity};
use crate::error::{Error, Result};

/// Settings for the shared client that reads remote media.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemoteSettings {
    pub(crate) connect_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) max_retries: u32,
    pub(crate) max_inflight_reads: usize,
    /// Permit resolved addresses that are loopback, private, or link-local.
    pub(crate) allow_private_addresses: bool,
}

/// The pooled client and the limits every remote read shares.
#[derive(Debug, Clone)]
pub(crate) struct RemoteReader {
    client: Client,
    permits: Arc<Semaphore>,
    settings: RemoteSettings,
}

impl RemoteReader {
    pub(crate) fn new(settings: RemoteSettings) -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(settings.connect_timeout)
            .timeout(settings.request_timeout)
            .pool_idle_timeout(Duration::from_secs(60))
            .dns_resolver(Arc::new(FilteringResolver {
                allow_private: settings.allow_private_addresses,
            }))
            .user_agent(concat!("vod-module-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| Error::Configuration(format!("remote media client: {error}")))?;
        Ok(Self {
            client,
            permits: Arc::new(Semaphore::new(settings.max_inflight_reads)),
            settings,
        })
    }

    /// Opens `url`: confirms it honors ranges, records its length, and captures a validator.
    pub(crate) async fn open(&self, url: Url) -> Result<HttpMediaSource> {
        let (validator, total) = self.probe(&url).await?;
        let mut display = url.clone();
        display.set_query(None);
        display.set_fragment(None);
        Ok(HttpMediaSource {
            reader: self.clone(),
            url: Mutex::new(url),
            refresher: OnceLock::new(),
            validator: validator.clone(),
            identity: SourceIdentity {
                origin: Origin::Remote {
                    url: display.to_string(),
                    validator: validator.value().to_owned(),
                },
                length: total,
                moov_sha256: None,
            },
        })
    }

    /// `Range: bytes=0-0`, expecting `206`, a total length, and a usable validator.
    async fn probe(&self, url: &Url) -> Result<(Validator, u64)> {
        let response = self
            .client
            .get(url.clone())
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(map_transport)?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(status_error(response.status(), "probing the media origin"));
        }
        let total = content_range(&response, 0, 0)?;
        let validator = Validator::from_headers(response.headers())?;
        Ok((validator, total))
    }

    fn should_retry(error: &Error) -> bool {
        matches!(error, Error::UpstreamUnavailable(_))
    }
}

/// Obtains a fresh URL for a source whose current one the origin has rejected, typically a
/// signed URL that expired or was revoked. Implemented by the registry, which re-asks the mapper.
pub(crate) trait LocationRefresher: Send + Sync + std::fmt::Debug {
    fn refresh(&self) -> Pin<Box<dyn Future<Output = Result<Url>> + Send + '_>>;
}

/// A remote object opened for ranged reads.
#[derive(Debug)]
pub(crate) struct HttpMediaSource {
    reader: RemoteReader,
    /// The URL reads currently use. Replaced when a signed URL is rotated, so streams already
    /// in flight pick up the new signature on their next read.
    url: Mutex<Url>,
    refresher: OnceLock<Arc<dyn LocationRefresher>>,
    validator: Validator,
    identity: SourceIdentity,
}

impl HttpMediaSource {
    pub(crate) fn identity(&self) -> &SourceIdentity {
        &self.identity
    }

    fn url(&self) -> Url {
        self.url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Points subsequent reads at `url`, for example a re-signed copy of the same object.
    pub(crate) fn set_url(&self, url: Url) {
        *self
            .url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = url;
    }

    /// Installs the hook used to recover from a rejected URL. Set once, after loading.
    pub(crate) fn set_refresher(&self, refresher: Arc<dyn LocationRefresher>) {
        let _ = self.refresher.set(refresher);
    }

    /// Reads exactly `range` with `If-Range`, retrying transient failures.
    pub(crate) async fn read_range(&self, range: ByteRange) -> Result<Bytes> {
        let end = range.end().filter(|end| *end <= self.identity.length);
        let Some(end) = end else {
            return Err(Error::InvalidRange {
                offset: range.offset,
                length: range.length,
                source_len: self.identity.length,
            });
        };
        if range.length == 0 {
            return Ok(Bytes::new());
        }
        let _permit = self
            .reader
            .permits
            .acquire()
            .await
            .map_err(|_| Error::UpstreamUnavailable("remote reads are shut down".to_owned()))?;
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            match self.fetch(range.offset, end - 1).await {
                // The origin refused the URL (an expired or revoked signature). Ask for a fresh
                // one once; a second rejection means the mapper cannot help.
                Err(Error::LocationRejected(message)) => {
                    let Some(hook) = self.refresher.get().filter(|_| !refreshed) else {
                        return Err(Error::LocationRejected(message));
                    };
                    refreshed = true;
                    self.set_url(hook.refresh().await?);
                }
                Err(error)
                    if RemoteReader::should_retry(&error)
                        && attempt < self.reader.settings.max_retries =>
                {
                    attempt += 1;
                    sleep(Duration::from_millis(50 * u64::from(attempt))).await;
                }
                other => return other,
            }
        }
    }

    async fn fetch(&self, first: u64, last: u64) -> Result<Bytes> {
        let mut response = self
            .reader
            .client
            .get(self.url())
            .header(RANGE, format!("bytes={first}-{last}"))
            .header(IF_RANGE, self.validator.value())
            .send()
            .await
            .map_err(map_transport)?;
        match response.status() {
            StatusCode::PARTIAL_CONTENT => {}
            // A full response to a conditional range means the validator no longer matches.
            StatusCode::OK | StatusCode::PRECONDITION_FAILED => {
                return Err(Error::InvalidMedia(
                    "remote source changed while it was being read".to_owned(),
                ));
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::GONE => {
                return Err(Error::LocationRejected(format!(
                    "{} while reading from the media origin",
                    response.status()
                )));
            }
            status => return Err(status_error(status, "reading from the media origin")),
        }
        if let Validator::ETag(expected) = &self.validator {
            let current = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok());
            if current != Some(expected.as_str()) {
                return Err(Error::InvalidMedia(
                    "remote source changed while it was being read".to_owned(),
                ));
            }
        }
        if content_range(&response, first, last)? != self.identity.length {
            return Err(Error::InvalidMedia(
                "remote source changed length while it was being read".to_owned(),
            ));
        }
        let expected = usize::try_from(last - first + 1)
            .map_err(|_| Error::Upstream("range does not fit in memory".to_owned()))?;
        let mut body = Vec::with_capacity(expected);
        while let Some(chunk) = response.chunk().await.map_err(map_transport)? {
            if body.len() + chunk.len() > expected {
                return Err(Error::Upstream(
                    "media origin returned more bytes than requested".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if body.len() != expected {
            return Err(Error::UpstreamUnavailable(
                "media origin returned a short body".to_owned(),
            ));
        }
        Ok(Bytes::from(body))
    }

    /// Re-probes the origin and compares length and validator with what was opened.
    pub(crate) async fn verify_unchanged(&self) -> Result<()> {
        let (validator, total) = self.reader.probe(&self.url()).await?;
        if validator == self.validator && total == self.identity.length {
            Ok(())
        } else {
            Err(Error::InvalidMedia(
                "remote source changed while it was being parsed".to_owned(),
            ))
        }
    }
}

/// What reads are conditioned on. A strong `ETag` is preferred; `Last-Modified` is the fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Validator {
    ETag(String),
    LastModified(String),
}

impl Validator {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Result<Self> {
        let strong_etag = headers
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .filter(|etag| !etag.starts_with("W/"));
        if let Some(etag) = strong_etag {
            return Ok(Self::ETag(etag.to_owned()));
        }
        headers
            .get(LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(|value| Self::LastModified(value.to_owned()))
            .ok_or_else(|| {
                Error::Upstream(
                    "media origin must send a strong ETag or a Last-Modified header".to_owned(),
                )
            })
    }

    fn value(&self) -> &str {
        match self {
            Self::ETag(value) | Self::LastModified(value) => value,
        }
    }
}

/// Parses `Content-Range: bytes first-last/total` and checks it matches the request.
fn content_range(response: &reqwest::Response, first: u64, last: u64) -> Result<u64> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| Error::Upstream("media origin sent no Content-Range".to_owned()))?;
    let bad = || Error::Upstream("media origin sent an unexpected Content-Range".to_owned());
    let rest = value.strip_prefix("bytes ").ok_or_else(bad)?;
    let (span, total) = rest.split_once('/').ok_or_else(bad)?;
    let (start, end) = span.split_once('-').ok_or_else(bad)?;
    let matches = start.parse::<u64>().ok() == Some(first) && end.parse::<u64>().ok() == Some(last);
    let total = total.parse::<u64>().map_err(|_| bad())?;
    if matches && total > last {
        Ok(total)
    } else {
        Err(bad())
    }
}

fn map_transport(error: reqwest::Error) -> Error {
    // The address filter reports a policy violation as a connect failure. Retrying it would only
    // repeat the violation, and it is not the origin's outage, so surface it as a bad upstream.
    let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&error);
    while let Some(inner) = cause {
        if inner
            .to_string()
            .contains("addresses that are not permitted")
        {
            return Error::Upstream(
                "location resolves to an address that is not permitted".to_owned(),
            );
        }
        cause = inner.source();
    }
    if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() {
        Error::UpstreamUnavailable(error.without_url().to_string())
    } else {
        Error::Upstream(error.without_url().to_string())
    }
}

fn status_error(status: StatusCode, action: &str) -> Error {
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        Error::UpstreamUnavailable(format!("{status} while {action}"))
    } else {
        Error::Upstream(format!("{status} while {action}"))
    }
}

/// Resolves names but drops addresses an untrusted location must not reach.
#[derive(Debug)]
struct FilteringResolver {
    allow_private: bool,
}

impl Resolve for FilteringResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let permitted = addresses
                .filter(|address| allow_private || is_public_address(address.ip()))
                .collect::<Vec<SocketAddr>>();
            if permitted.is_empty() {
                return Err("host resolves only to addresses that are not permitted".into());
            }
            Ok(Box::new(permitted.into_iter()) as Addrs)
        })
    }
}

/// True for addresses that belong to the public internet.
///
/// Rejects loopback, private, link-local, shared (CGNAT), multicast, unspecified, documentation,
/// and unique-local ranges, and IPv4-mapped IPv6 addresses whose IPv4 part is in any of them.
pub(crate) fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        || address.is_documentation()
        || octets[0] == 0
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        || octets[0] >= 240)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first == 0x2001 && address.segments()[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
            "ff02::1",
        ] {
            assert!(
                !is_public_address(private.parse().unwrap()),
                "{private} should be refused"
            );
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(
                is_public_address(public.parse().unwrap()),
                "{public} should be allowed"
            );
        }
    }
}
