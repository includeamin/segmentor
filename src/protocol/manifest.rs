//! A rendered playlist or manifest, kept with its compressed forms.
//!
//! Playlists are text that repeats itself line after line, so they compress by a factor of ten
//! or more, and players and browsers all accept gzip. Each form is made once, on the first
//! request that asks for it, and served from memory after that: the cost is one compression
//! per playlist per asset load, never one per request.

use std::io::Write;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;

/// A content coding a client can be sent, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Brotli,
    Gzip,
    Identity,
}

impl Encoding {
    /// The `Content-Encoding` value, or `None` for identity.
    pub(crate) fn header(self) -> Option<&'static str> {
        match self {
            Self::Brotli => Some("br"),
            Self::Gzip => Some("gzip"),
            Self::Identity => None,
        }
    }

    /// The best coding an `Accept-Encoding` value allows (RFC 9110 §12.5.3).
    ///
    /// A coding is acceptable when it is listed, or covered by `*`, with a nonzero `q`. Among
    /// acceptable codings brotli is preferred, then gzip, whatever their `q`: both are lossless
    /// and brotli is the smaller. Identity is always the fallback. A client that refuses
    /// identity and accepts neither coding still gets identity, as a client that sent no
    /// header would, rather than an error.
    pub(crate) fn negotiate<'a>(accept_encoding: impl IntoIterator<Item = &'a str>) -> Self {
        let mut wildcard = None;
        let mut brotli = None;
        let mut gzip = None;
        for value in accept_encoding {
            for item in value.split(',') {
                let mut parts = item.split(';');
                let coding = parts.next().unwrap_or_default().trim();
                let acceptable = parts
                    .filter_map(|parameter| {
                        let (name, value) = parameter.split_once('=')?;
                        name.trim().eq_ignore_ascii_case("q").then(|| value.trim())
                    })
                    .next_back()
                    .is_none_or(|q| q.parse::<f32>().is_ok_and(|q| q > 0.0));
                if coding.eq_ignore_ascii_case("br") {
                    brotli = Some(acceptable);
                } else if coding.eq_ignore_ascii_case("gzip")
                    || coding.eq_ignore_ascii_case("x-gzip")
                {
                    gzip = Some(acceptable);
                } else if coding == "*" {
                    wildcard = Some(acceptable);
                }
            }
        }
        let wildcard = wildcard.unwrap_or(false);
        if brotli.unwrap_or(wildcard) {
            Self::Brotli
        } else if gzip.unwrap_or(wildcard) {
            Self::Gzip
        } else {
            Self::Identity
        }
    }
}

/// A rendered playlist or manifest. Cloning it is cheap and shares the compressed forms.
#[derive(Clone, Debug)]
pub(crate) struct Manifest(Arc<Forms>);

#[derive(Debug)]
struct Forms {
    identity: Bytes,
    gzip: OnceLock<Bytes>,
    brotli: OnceLock<Bytes>,
}

impl Manifest {
    pub(crate) fn new(identity: impl Into<Bytes>) -> Self {
        Self(Arc::new(Forms {
            identity: identity.into(),
            gzip: OnceLock::new(),
            brotli: OnceLock::new(),
        }))
    }

    /// The body to send for `encoding`, compressing it on first use.
    pub(crate) fn encoded(&self, encoding: Encoding) -> Bytes {
        match encoding {
            Encoding::Identity => self.0.identity.clone(),
            Encoding::Gzip => self.0.gzip.get_or_init(|| gzip(&self.0.identity)).clone(),
            Encoding::Brotli => self
                .0
                .brotli
                .get_or_init(|| brotli(&self.0.identity))
                .clone(),
        }
    }

    /// Bytes held by every form made so far, for cache accounting.
    pub(crate) fn len(&self) -> usize {
        self.0.identity.len()
            + self.0.gzip.get().map_or(0, Bytes::len)
            + self.0.brotli.get().map_or(0, Bytes::len)
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new(Bytes::new())
    }
}

impl From<String> for Manifest {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl std::ops::Deref for Manifest {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0.identity
    }
}

/// Level 6, zlib's default: within a few percent of level 9 on playlists, at a fraction of
/// the time.
fn gzip(text: &[u8]) -> Bytes {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(text.len() / 8),
        flate2::Compression::new(6),
    );
    encoder
        .write_all(text)
        .expect("writing to a Vec cannot fail");
    Bytes::from(encoder.finish().expect("writing to a Vec cannot fail"))
}

/// Quality 4: on a 600-segment playlist it is as small as quality 9 (849 against 848 bytes,
/// from 34 KB) in a fraction of the time, and the first request for each playlist waits on it.
/// Quality 11 is no smaller and a hundred times slower.
fn brotli(text: &[u8]) -> Bytes {
    let mut output = Vec::with_capacity(text.len() / 8);
    let parameters = ::brotli::enc::BrotliEncoderParams {
        quality: 4,
        lgwin: 22,
        size_hint: text.len(),
        ..::brotli::enc::BrotliEncoderParams::default()
    };
    ::brotli::BrotliCompress(&mut &text[..], &mut output, &parameters)
        .expect("reading from a slice and writing to a Vec cannot fail");
    Bytes::from(output)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    fn negotiate(header: &str) -> Encoding {
        Encoding::negotiate([header])
    }

    #[test]
    fn brotli_is_preferred_then_gzip_then_identity() {
        assert_eq!(negotiate("gzip, deflate, br, zstd"), Encoding::Brotli);
        assert_eq!(negotiate("gzip, deflate"), Encoding::Gzip);
        assert_eq!(negotiate("x-gzip"), Encoding::Gzip);
        assert_eq!(negotiate("deflate"), Encoding::Identity);
        assert_eq!(negotiate(""), Encoding::Identity);
        assert_eq!(Encoding::negotiate([]), Encoding::Identity);
    }

    #[test]
    fn a_zero_quality_refuses_a_coding() {
        assert_eq!(negotiate("br;q=0, gzip"), Encoding::Gzip);
        assert_eq!(negotiate("br; q=0.0, gzip;q=0"), Encoding::Identity);
        assert_eq!(negotiate("gzip;q=0.5, br;q=0.1"), Encoding::Brotli);
        assert_eq!(negotiate("GZIP;Q=1"), Encoding::Gzip);
    }

    #[test]
    fn a_wildcard_covers_codings_not_listed() {
        assert_eq!(negotiate("*"), Encoding::Brotli);
        assert_eq!(negotiate("br;q=0, *"), Encoding::Gzip);
        assert_eq!(negotiate("*;q=0, gzip"), Encoding::Gzip);
        assert_eq!(negotiate("*;q=0"), Encoding::Identity);
    }

    #[test]
    fn several_header_lines_are_combined() {
        assert_eq!(Encoding::negotiate(["deflate", "gzip"]), Encoding::Gzip);
    }

    #[test]
    fn every_form_decompresses_to_the_text_and_is_made_once() {
        let text = "#EXTINF:6.000,\nsegments/0/media.m4s?v=0123456789abcdef\n".repeat(600);
        let manifest = Manifest::from(text.clone());
        assert_eq!(manifest.len(), text.len());

        let gzipped = manifest.encoded(Encoding::Gzip);
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(&gzipped[..])
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, text);

        let brotli = manifest.encoded(Encoding::Brotli);
        let mut decoded = String::new();
        ::brotli::Decompressor::new(&brotli[..], 4096)
            .read_to_string(&mut decoded)
            .unwrap();
        assert_eq!(decoded, text);

        assert!(gzipped.len() * 10 < text.len());
        assert!(brotli.len() * 10 < text.len());
        assert_eq!(manifest.len(), text.len() + gzipped.len() + brotli.len());
        // Made once: the second request gets the same allocation.
        assert_eq!(manifest.encoded(Encoding::Gzip).as_ptr(), gzipped.as_ptr());
        assert_eq!(manifest.encoded(Encoding::Identity), text.as_bytes());
    }
}
