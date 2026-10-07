//! Signed playback tokens (TDD 0007, Mode 1): verified locally, on every request, with no network
//! call.
//!
//! A token is a JWT, restricted tightly rather than accepted broadly. Exactly one algorithm is
//! configured (`HS256` with a shared secret, or `ES256` or `EdDSA` with a public key), the token's
//! header must name exactly that algorithm, and anything else, `alg: none` included, is refused
//! outright: there is no negotiation and no fallback. The signature is checked before the claims
//! are read. Nothing here knows about accounts or subscriptions; a token is an opaque, scoped
//! grant that names what it covers.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::hmac;
use aws_lc_rs::signature::{self, UnparsedPublicKey};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::Deserialize;

use crate::config::Secret;

/// The most assets one token may name: a short explicit list, not a catalog.
const MAX_ASSETS: usize = 64;
/// The most renditions or languages one token may name.
const MAX_SCOPES: usize = 256;

/// The one algorithm a deployment accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Algorithm {
    Hs256,
    Es256,
    EdDsa,
}

impl Algorithm {
    /// The `alg` header value.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Hs256 => "HS256",
            Self::Es256 => "ES256",
            Self::EdDsa => "EdDSA",
        }
    }
}

/// The key tokens are checked against.
#[derive(Clone)]
pub(crate) enum Key {
    /// A shared secret, for `HS256`.
    Secret(Secret),
    /// An uncompressed P-256 point (65 bytes), for `ES256`.
    P256(Vec<u8>),
    /// A 32-byte Ed25519 public key, for `EdDSA`.
    Ed25519(Vec<u8>),
}

impl fmt::Debug for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Secret(_) => "Key::Secret(<redacted>)",
            Self::P256(_) => "Key::P256",
            Self::Ed25519(_) => "Key::Ed25519",
        })
    }
}

/// `SubjectPublicKeyInfo` for a P-256 key ends in the 65-byte uncompressed point; this is
/// everything before it (RFC 5480: `id-ecPublicKey`, `prime256v1`, and the bit string header).
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
/// The same for Ed25519 (RFC 8410), ending in the 32-byte key.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

impl Key {
    /// The key for `algorithm` from what the operator supplied: for `HS256`, the secret itself;
    /// for the others, a PEM-encoded `SubjectPublicKeyInfo`.
    pub(crate) fn parse(algorithm: Algorithm, material: &str) -> Result<Self, &'static str> {
        match algorithm {
            Algorithm::Hs256 => {
                if material.len() < 32 {
                    return Err("an HS256 secret must be at least 32 bytes");
                }
                Ok(Self::Secret(Secret::new(material.to_owned())))
            }
            Algorithm::Es256 => {
                let point = spki_key(material, &P256_SPKI_PREFIX, 65)
                    .ok_or("the public key is not a PEM P-256 SubjectPublicKeyInfo")?;
                if point.first() != Some(&0x04) {
                    return Err("the public key is not an uncompressed P-256 point");
                }
                Ok(Self::P256(point))
            }
            Algorithm::EdDsa => {
                let key = spki_key(material, &ED25519_SPKI_PREFIX, 32)
                    .ok_or("the public key is not a PEM Ed25519 SubjectPublicKeyInfo")?;
                Ok(Self::Ed25519(key))
            }
        }
    }
}

/// The key bytes of a PEM `PUBLIC KEY` whose DER is exactly `prefix` followed by `length` bytes.
fn spki_key(pem: &str, prefix: &[u8], length: usize) -> Option<Vec<u8>> {
    let mut lines = pem.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next()? != "-----BEGIN PUBLIC KEY-----" {
        return None;
    }
    let mut encoded = String::new();
    loop {
        let line = lines.next()?;
        if line == "-----END PUBLIC KEY-----" {
            break;
        }
        encoded.push_str(line);
    }
    if lines.next().is_some() {
        return None;
    }
    let der = STANDARD.decode(encoded).ok()?;
    (der.len() == prefix.len() + length && der.starts_with(prefix))
        .then(|| der[prefix.len()..].to_vec())
}

/// Why a token was refused. The reasons are for metrics and the debug log; the response says only
/// what a caller may know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Denial {
    /// No token, or one that is not three base64url parts with readable header and claims.
    Malformed,
    /// Larger than `max_token_bytes`: refused before anything is parsed.
    TooLarge,
    /// The header names another algorithm than the configured one, or has `crit`.
    WrongAlgorithm,
    BadSignature,
    Expired,
    NotYetValid,
    /// A valid token that does not cover what was asked for.
    NotCovered,
}

impl Denial {
    /// What a caller is told (never anything about the asset): `401` for a credential problem.
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::Expired => "token expired",
            Self::NotYetValid => "token not yet valid",
            Self::NotCovered => "the token does not cover this resource",
            Self::Malformed | Self::TooLarge | Self::WrongAlgorithm | Self::BadSignature => {
                "invalid token"
            }
        }
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    /// A critical extension this does not understand makes the token unacceptable (RFC 7515,
    /// 4.1.11); none is understood.
    crit: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Claims {
    exp: Option<serde_json::Number>,
    nbf: Option<serde_json::Number>,
    asset: Option<OneOrMany>,
    renditions: Option<Vec<String>>,
    subtitles: Option<Vec<String>>,
}

/// `"asset": "movie"` or `"asset": ["movie", "trailer"]`.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// What a verified token grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    assets: Vec<String>,
    /// `None` covers every rendition.
    renditions: Option<Vec<String>>,
    /// `None` covers every language.
    subtitles: Option<Vec<String>>,
}

/// What a request asks for, read from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Resource<'a> {
    pub(crate) asset: &'a str,
    /// The id after `video-` in a rendition's URL; `None` for the master playlist, a plain
    /// asset's tracks, and the shared audio group, which no rendition scope applies to.
    pub(crate) rendition: Option<&'a str>,
    pub(crate) subtitle: Option<&'a str>,
}

impl<'a> Resource<'a> {
    /// The resource a `/hls/...` or `/dash/...` path names, or `None` for any other path.
    pub(crate) fn from_path(path: &'a str) -> Option<Self> {
        let mut segments = path.strip_prefix('/')?.split('/');
        if !matches!(segments.next()?, "hls" | "dash") {
            return None;
        }
        let asset = segments.next().filter(|asset| !asset.is_empty())?;
        let mut resource = Self {
            asset,
            rendition: None,
            subtitle: None,
        };
        match segments.next() {
            Some("subtitles") => resource.subtitle = segments.next(),
            Some(track) => resource.rendition = track.strip_prefix("video-"),
            None => {}
        }
        Some(resource)
    }
}

impl Grant {
    /// Whether this grant covers `resource`: its asset, and, if the grant lists renditions or
    /// languages, the one asked for.
    pub(crate) fn covers(&self, resource: &Resource<'_>) -> bool {
        self.assets.iter().any(|asset| asset == resource.asset)
            && match (&self.renditions, resource.rendition) {
                (Some(allowed), Some(rendition)) => allowed.iter().any(|id| id == rendition),
                _ => true,
            }
            && match (&self.subtitles, resource.subtitle) {
                (Some(allowed), Some(language)) => allowed
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(language)),
                _ => true,
            }
    }
}

/// The settings and key tokens are verified with.
#[derive(Debug, Clone)]
pub(crate) struct Verifier {
    algorithm: Algorithm,
    key: Key,
    skew: Duration,
    max_token_bytes: usize,
}

impl Verifier {
    pub(crate) fn new(
        algorithm: Algorithm,
        key: Key,
        skew: Duration,
        max_token_bytes: usize,
    ) -> Self {
        Self {
            algorithm,
            key,
            skew,
            max_token_bytes,
        }
    }

    /// Checks `token` and returns what it grants, at time `now`.
    pub(crate) fn verify(&self, token: &str, now: SystemTime) -> Result<Grant, Denial> {
        if token.len() > self.max_token_bytes {
            return Err(Denial::TooLarge);
        }
        let mut parts = token.split('.');
        let (Some(header), Some(claims), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Denial::Malformed);
        };
        if [header, claims, signature]
            .iter()
            .any(|part| part.is_empty())
        {
            return Err(Denial::Malformed);
        }
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| Denial::Malformed);

        let header: Header =
            serde_json::from_slice(&decode(header)?).map_err(|_| Denial::Malformed)?;
        if header.alg != self.algorithm.name() || header.crit.is_some() {
            return Err(Denial::WrongAlgorithm);
        }
        let signed = &token[..header_and_claims_length(token)];
        self.check_signature(signed.as_bytes(), &decode(signature)?)?;

        let claims: Claims =
            serde_json::from_slice(&decode(claims)?).map_err(|_| Denial::Malformed)?;
        self.check_times(&claims, now)?;
        let assets = match claims.asset {
            Some(OneOrMany::One(asset)) => vec![asset],
            Some(OneOrMany::Many(assets)) => assets,
            None => return Err(Denial::Malformed),
        };
        let within = |list: &[String], most: usize| list.len() <= most;
        if assets.is_empty()
            || !within(&assets, MAX_ASSETS)
            || claims
                .renditions
                .as_deref()
                .is_some_and(|list| !within(list, MAX_SCOPES))
            || claims
                .subtitles
                .as_deref()
                .is_some_and(|list| !within(list, MAX_SCOPES))
        {
            return Err(Denial::Malformed);
        }
        Ok(Grant {
            assets,
            renditions: claims.renditions,
            subtitles: claims.subtitles,
        })
    }

    fn check_signature(&self, signed: &[u8], signature: &[u8]) -> Result<(), Denial> {
        let valid = match &self.key {
            Key::Secret(secret) => {
                let key = hmac::Key::new(hmac::HMAC_SHA256, secret.expose().as_bytes());
                hmac::verify(&key, signed, signature).is_ok()
            }
            Key::P256(point) => UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                .verify(signed, signature)
                .is_ok(),
            Key::Ed25519(key) => UnparsedPublicKey::new(&signature::ED25519, key)
                .verify(signed, signature)
                .is_ok(),
        };
        if valid {
            Ok(())
        } else {
            Err(Denial::BadSignature)
        }
    }

    fn check_times(&self, claims: &Claims, now: SystemTime) -> Result<(), Denial> {
        let now = now
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let skew = self.skew.as_secs_f64();
        let seconds =
            |number: &serde_json::Number| number.as_f64().filter(|value| value.is_finite());
        // `exp` is required: a token with no end is not a short-lived grant.
        let expires = claims
            .exp
            .as_ref()
            .and_then(seconds)
            .ok_or(Denial::Malformed)?;
        if now >= expires + skew {
            return Err(Denial::Expired);
        }
        if let Some(not_before) = claims.nbf.as_ref() {
            let not_before = seconds(not_before).ok_or(Denial::Malformed)?;
            if now + skew < not_before {
                return Err(Denial::NotYetValid);
            }
        }
        Ok(())
    }
}

/// The length of `header.claims` within a three-part token: what the signature covers.
fn header_and_claims_length(token: &str) -> usize {
    token.rfind('.').unwrap_or(token.len())
}

/// Minting tokens, for tests of the code that checks them. segmentor never issues credentials.
#[cfg(test)]
pub(crate) mod testing {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use aws_lc_rs::hmac;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    pub(crate) const SECRET: &str = "test-secret-of-at-least-32-bytes-long!";

    /// The current time in seconds, plus `delta`.
    pub(crate) fn seconds_from_now(delta: i64) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        now.saturating_add_signed(delta)
    }

    /// An `HS256` token under [`SECRET`] with these claims.
    pub(crate) fn mint(claims: &serde_json::Value) -> String {
        let encode = |value: &serde_json::Value| {
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("JSON serializes"))
        };
        let signed = format!(
            "{}.{}",
            encode(&serde_json::json!({ "alg": "HS256", "typ": "JWT" })),
            encode(claims)
        );
        let key = hmac::Key::new(hmac::HMAC_SHA256, SECRET.as_bytes());
        format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(hmac::sign(&key, signed.as_bytes()).as_ref())
        )
    }

    /// A token for `asset` that expires in an hour.
    pub(crate) fn mint_for(asset: &str) -> String {
        mint(&serde_json::json!({ "exp": seconds_from_now(3600), "asset": asset }))
    }
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair,
    };

    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123";
    const NOW: u64 = 1_800_000_000;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn encode(value: &serde_json::Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
    }

    /// A token signed with `HS256` under `SECRET`.
    fn hs256(header: &serde_json::Value, claims: &serde_json::Value) -> String {
        let signed = format!("{}.{}", encode(header), encode(claims));
        let key = hmac::Key::new(hmac::HMAC_SHA256, SECRET.as_bytes());
        let tag = hmac::sign(&key, signed.as_bytes());
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()))
    }

    fn header(alg: &str) -> serde_json::Value {
        serde_json::json!({ "alg": alg, "typ": "JWT" })
    }

    fn claims() -> serde_json::Value {
        serde_json::json!({ "exp": NOW + 60, "asset": "movie" })
    }

    fn verifier() -> Verifier {
        Verifier::new(
            Algorithm::Hs256,
            Key::parse(Algorithm::Hs256, SECRET).unwrap(),
            Duration::from_secs(30),
            4096,
        )
    }

    fn verify(token: &str) -> Result<Grant, Denial> {
        verifier().verify(token, at(NOW))
    }

    #[test]
    fn a_valid_token_grants_what_it_names() {
        let token = hs256(&header("HS256"), &claims());

        let grant = verify(&token).unwrap();

        assert!(grant.covers(&Resource::from_path("/hls/movie/master.m3u8").unwrap()));
        assert!(!grant.covers(&Resource::from_path("/hls/other/master.m3u8").unwrap()));
    }

    #[test]
    fn one_asset_or_a_short_list_and_no_patterns() {
        let many = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": ["a", "b"] }),
        );
        let grant = verify(&many).unwrap();
        for (path, covered) in [
            ("/hls/a/master.m3u8", true),
            ("/dash/b/manifest.mpd", true),
            ("/hls/c/master.m3u8", false),
        ] {
            assert_eq!(
                grant.covers(&Resource::from_path(path).unwrap()),
                covered,
                "{path}"
            );
        }
        // A grant names what it covers: a wildcard is just a (wrong) asset ID.
        let star = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": "*" }),
        );
        assert!(
            !verify(&star)
                .unwrap()
                .covers(&Resource::from_path("/hls/movie/master.m3u8").unwrap())
        );
        let many_assets = (0..=MAX_ASSETS).map(|n| n.to_string()).collect::<Vec<_>>();
        let too_many = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": many_assets }),
        );
        assert_eq!(verify(&too_many), Err(Denial::Malformed));
        let none = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": [] }),
        );
        assert_eq!(verify(&none), Err(Denial::Malformed));
    }

    #[test]
    fn renditions_and_languages_narrow_a_grant_only_when_listed() {
        let token = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": "movie", "renditions": ["720p"], "subtitles": ["en", "fr"] }),
        );
        let grant = verify(&token).unwrap();
        let covers = |path: &str| grant.covers(&Resource::from_path(path).unwrap());

        assert!(covers("/hls/movie/video-720p/index.m3u8"));
        assert!(covers("/dash/movie/video-720p/segments/3/media.m4s"));
        assert!(!covers("/hls/movie/video-1080p/index.m3u8"));
        // The master, a plain asset's tracks, and the shared audio group are not renditions.
        assert!(covers("/hls/movie/master.m3u8"));
        assert!(covers("/hls/movie/video/index.m3u8"));
        assert!(covers("/hls/movie/audio-1/index.m3u8"));
        assert!(covers("/hls/movie/subtitles/en/sub.vtt"));
        assert!(covers("/hls/movie/subtitles/FR/sub.vtt"));
        assert!(!covers("/hls/movie/subtitles/de/sub.vtt"));
        // Absent lists cover everything.
        let open = verify(&hs256(&header("HS256"), &claims())).unwrap();
        assert!(open.covers(&Resource::from_path("/hls/movie/video-anything/index.m3u8").unwrap()));
        assert!(open.covers(&Resource::from_path("/hls/movie/subtitles/de/sub.vtt").unwrap()));
    }

    #[test]
    fn times_are_enforced_with_the_configured_skew() {
        let token = |exp: u64, nbf: Option<u64>| {
            let mut claims = serde_json::json!({ "exp": exp, "asset": "movie" });
            if let Some(nbf) = nbf {
                claims["nbf"] = nbf.into();
            }
            hs256(&header("HS256"), &claims)
        };
        let check = |token: &str, now: u64| verifier().verify(token, at(now));

        // Expired once the skew (30 s) is also used up.
        assert!(check(&token(NOW, None), NOW + 29).is_ok());
        assert_eq!(check(&token(NOW, None), NOW + 30), Err(Denial::Expired));
        // Not yet valid until the skew before `nbf`.
        assert!(check(&token(NOW + 500, Some(NOW + 100)), NOW + 70).is_ok());
        assert_eq!(
            check(&token(NOW + 500, Some(NOW + 100)), NOW + 69),
            Err(Denial::NotYetValid)
        );
        // `exp` is required, and must be a number.
        let missing = hs256(&header("HS256"), &serde_json::json!({ "asset": "movie" }));
        assert_eq!(verify(&missing), Err(Denial::Malformed));
        let text = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": "soon", "asset": "movie" }),
        );
        assert_eq!(verify(&text), Err(Denial::Malformed));
        // A fractional expiry is read as seconds.
        let fractional = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": 1_800_000_000.5_f64, "asset": "movie" }),
        );
        assert!(verify(&fractional).is_ok());
    }

    #[test]
    fn only_the_configured_algorithm_is_accepted() {
        // `none`, whatever the signature part says, and the other algorithms' names.
        for alg in [
            "none", "None", "NONE", "HS384", "HS512", "RS256", "ES256", "EdDSA", "hs256", "",
        ] {
            let token = hs256(&header(alg), &claims());
            assert_eq!(verify(&token), Err(Denial::WrongAlgorithm), "{alg:?}");
        }
        let unsigned = format!("{}.{}.", encode(&header("none")), encode(&claims()));
        assert_eq!(verify(&unsigned), Err(Denial::Malformed));
        // A header extension that must be understood, and is not.
        let crit = hs256(
            &serde_json::json!({ "alg": "HS256", "crit": ["b64"], "b64": false }),
            &claims(),
        );
        assert_eq!(verify(&crit), Err(Denial::WrongAlgorithm));
    }

    #[test]
    fn a_public_key_cannot_be_used_as_an_hmac_secret() {
        // The classic confusion: sign with HS256 using the public key's bytes as the secret, and
        // present it to a verifier configured for ES256. The header names HS256, which this
        // verifier does not accept, whatever the key.
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let rng_key = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, rng_key.as_ref()).unwrap();
        let point = pair.public_key().as_ref().to_vec();
        let es256 = Verifier::new(
            Algorithm::Es256,
            Key::P256(point.clone()),
            Duration::from_secs(30),
            4096,
        );
        let signed = format!("{}.{}", encode(&header("HS256")), encode(&claims()));
        let forged = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, &point),
            signed.as_bytes(),
        );
        let token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(forged.as_ref()));

        assert_eq!(es256.verify(&token, at(NOW)), Err(Denial::WrongAlgorithm));
    }

    #[test]
    fn a_changed_token_is_refused() {
        let token = hs256(&header("HS256"), &claims());
        let parts = token.split('.').collect::<Vec<_>>();
        // Claims changed after signing.
        let other = encode(&serde_json::json!({ "exp": NOW + 99_999, "asset": "movie" }));
        assert_eq!(
            verify(&format!("{}.{other}.{}", parts[0], parts[2])),
            Err(Denial::BadSignature)
        );
        // One bit of the signature, and a signature of the wrong length.
        let mut signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        signature[0] ^= 1;
        assert_eq!(
            verify(&format!(
                "{}.{}.{}",
                parts[0],
                parts[1],
                URL_SAFE_NO_PAD.encode(&signature)
            )),
            Err(Denial::BadSignature)
        );
        assert_eq!(
            verify(&format!(
                "{}.{}.{}",
                parts[0],
                parts[1],
                URL_SAFE_NO_PAD.encode(&signature[..16])
            )),
            Err(Denial::BadSignature)
        );
        // A different secret.
        let other_secret = Verifier::new(
            Algorithm::Hs256,
            Key::parse(
                Algorithm::Hs256,
                "a-completely-different-secret-of-32+bytes",
            )
            .unwrap(),
            Duration::ZERO,
            4096,
        );
        assert_eq!(
            other_secret.verify(&token, at(NOW)),
            Err(Denial::BadSignature)
        );
    }

    #[test]
    fn what_is_not_a_token_is_refused_before_anything_else() {
        let good = hs256(&header("HS256"), &claims());
        for bad in [
            "",
            "abc",
            "a.b",
            "a.b.c.d",
            "..",
            "a..c",
            ".b.c",
            "not base64!.b.c",
            // Padding and the standard alphabet are not base64url without padding.
            &format!(
                "{}=.{}.{}",
                encode(&header("HS256")),
                encode(&claims()),
                "AA"
            ),
            &good.replace('-', "+").replace('_', "/")[..good.len() - 1],
        ] {
            assert!(
                matches!(
                    verify(bad),
                    Err(Denial::Malformed | Denial::BadSignature | Denial::WrongAlgorithm)
                ),
                "{bad:?}"
            );
        }
        // Not JSON, and JSON of the wrong shape.
        let not_json = format!(
            "{}.{}.AA",
            URL_SAFE_NO_PAD.encode("nope"),
            encode(&claims())
        );
        assert_eq!(verify(&not_json), Err(Denial::Malformed));
        let array_header = format!(
            "{}.{}.AA",
            encode(&serde_json::json!([1])),
            encode(&claims())
        );
        assert_eq!(verify(&array_header), Err(Denial::Malformed));
        // An oversized token is refused unread.
        let huge = "a".repeat(5000);
        assert_eq!(verify(&huge), Err(Denial::TooLarge));
        let padded = hs256(
            &header("HS256"),
            &serde_json::json!({ "exp": NOW + 60, "asset": "x".repeat(5000) }),
        );
        assert_eq!(verify(&padded), Err(Denial::TooLarge));
    }

    #[test]
    fn asset_paths_name_their_resource() {
        fn parse(path: &str) -> Option<Resource<'_>> {
            Resource::from_path(path)
        }
        assert_eq!(
            parse("/hls/movie/master.m3u8"),
            Some(Resource {
                asset: "movie",
                rendition: None,
                subtitle: None
            })
        );
        assert_eq!(
            parse("/dash/movie/video-480p/init.mp4"),
            Some(Resource {
                asset: "movie",
                rendition: Some("480p"),
                subtitle: None
            })
        );
        assert_eq!(
            parse("/hls/movie/subtitles/en/index.m3u8"),
            Some(Resource {
                asset: "movie",
                rendition: None,
                subtitle: Some("en")
            })
        );
        for not_media in [
            "/health",
            "/ready",
            "/metrics",
            "/admin/status",
            "/",
            "/hls",
            "/hls/",
            "/hls//x",
            "/other/a/b",
        ] {
            assert_eq!(parse(not_media), None, "{not_media}");
        }
    }

    /// `openssl` makes the keys, so the PEM parsing and the signatures are checked against an
    /// independent implementation.
    #[test]
    fn es256_and_eddsa_keys_from_openssl_verify_tokens() {
        use std::process::Command;

        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("skipping: openssl is not installed");
            return;
        }
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/auth-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let openssl = |arguments: &[&str]| {
            let output = Command::new("openssl").args(arguments).output().unwrap();
            assert!(
                output.status.success(),
                "{arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        for (algorithm, openssl_algorithm) in
            [(Algorithm::Es256, "EC"), (Algorithm::EdDsa, "ED25519")]
        {
            let private =
                directory.join(format!("{}-{}.key", algorithm.name(), std::process::id()));
            let mut generate = vec![
                "genpkey",
                "-algorithm",
                openssl_algorithm,
                "-outform",
                "DER",
                "-out",
                private.to_str().unwrap(),
            ];
            if algorithm == Algorithm::Es256 {
                generate.extend(["-pkeyopt", "ec_paramgen_curve:P-256"]);
            }
            openssl(&generate);
            let public_pem = String::from_utf8(openssl(&[
                "pkey",
                "-inform",
                "DER",
                "-in",
                private.to_str().unwrap(),
                "-pubout",
            ]))
            .unwrap();
            // `genpkey` writes an EC key as a traditional SEC1 structure; the signing library reads
            // PKCS#8, so OpenSSL converts it (Ed25519 is PKCS#8 already, and converts to itself).
            let pkcs8 = openssl(&[
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-inform",
                "DER",
                "-outform",
                "DER",
                "-in",
                private.to_str().unwrap(),
            ]);
            std::fs::remove_file(&private).unwrap();

            let signed = format!(
                "{}.{}",
                encode(&header(algorithm.name())),
                encode(&claims())
            );
            let signature = match algorithm {
                Algorithm::Es256 => {
                    let pair =
                        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8).unwrap();
                    let rng = aws_lc_rs::rand::SystemRandom::new();
                    pair.sign(&rng, signed.as_bytes())
                        .unwrap()
                        .as_ref()
                        .to_vec()
                }
                _ => Ed25519KeyPair::from_pkcs8(&pkcs8)
                    .unwrap()
                    .sign(signed.as_bytes())
                    .as_ref()
                    .to_vec(),
            };
            let token = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
            let verifier = Verifier::new(
                algorithm,
                Key::parse(algorithm, &public_pem)
                    .unwrap_or_else(|reason| panic!("{reason}\n{public_pem}")),
                Duration::from_secs(30),
                4096,
            );

            assert!(
                verifier.verify(&token, at(NOW)).is_ok(),
                "{}",
                algorithm.name()
            );
            // A token for the other key is refused.
            let mut tampered = token.into_bytes();
            let last = tampered.len() - 2;
            tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
            assert!(
                verifier
                    .verify(&String::from_utf8(tampered).unwrap(), at(NOW))
                    .is_err()
            );
        }
    }

    #[test]
    fn keys_that_are_not_what_was_configured_are_rejected() {
        assert!(Key::parse(Algorithm::Hs256, "too short").is_err());
        let not_pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        assert!(Key::parse(Algorithm::Es256, not_pem).is_err());
        assert!(Key::parse(Algorithm::EdDsa, "").is_err());
        // A valid base64 body of the wrong kind of key (an Ed25519 key offered for ES256).
        let mut der = ED25519_SPKI_PREFIX.to_vec();
        der.extend([7; 32]);
        let pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            STANDARD.encode(der)
        );
        assert!(Key::parse(Algorithm::EdDsa, &pem).is_ok());
        assert!(Key::parse(Algorithm::Es256, &pem).is_err());
        // Text after the key, or before it.
        assert!(Key::parse(Algorithm::EdDsa, &format!("{pem}extra")).is_err());
        assert!(!format!("{:?}", Key::parse(Algorithm::Hs256, SECRET).unwrap()).contains("0123"));
    }
}
