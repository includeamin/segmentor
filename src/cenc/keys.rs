//! Content keys and DRM systems, as the mapper supplies them, validated before any use.

use std::fmt::{self, Write};

use base64::Engine;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::media::TrackKind;
use crate::mp4::boxes::Reader;

pub(crate) const WIDEVINE: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];
pub(crate) const FAIRPLAY: [u8; 16] = [
    0x94, 0xce, 0x86, 0xfb, 0x07, 0xff, 0x4f, 0x43, 0xad, 0xb8, 0x93, 0xd2, 0xfa, 0x96, 0x8c, 0xa2,
];
pub(crate) const PLAYREADY: [u8; 16] = [
    0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f, 0x95,
];
pub(crate) const CLEARKEY: [u8; 16] = [
    0xe2, 0x71, 0x9d, 0x58, 0xa9, 0x85, 0xb3, 0xc9, 0x78, 0x1a, 0xb0, 0x30, 0xaf, 0x78, 0xd3, 0x0e,
];

const MAX_SYSTEMS: usize = 8;
const MAX_PSSH_BYTES: usize = 16 * 1024;
const MAX_URI_BYTES: usize = 2048;

/// Sixteen bytes of content key. `Debug` never prints them, so no log line or panic can.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct KeyBytes([u8; 16]);

impl KeyBytes {
    pub(crate) const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(crate) const fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for KeyBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KeyBytes(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContentKey {
    pub(crate) key_id: [u8; 16],
    pub(crate) key: KeyBytes,
    /// The constant IV declared in `tenc`, restarted at every protected range.
    pub(crate) iv: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Keys {
    All(ContentKey),
    Split {
        video: ContentKey,
        audio: ContentKey,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DrmSystem {
    pub(crate) system_id: [u8; 16],
    /// A complete, validated `pssh` box.
    pub(crate) pssh: Option<Vec<u8>>,
    pub(crate) license_url: Option<String>,
    pub(crate) hls_uri: Option<String>,
}

/// What the mapper's `encryption` object asks for, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Encryption {
    pub(crate) keys: Keys,
    pub(crate) systems: Vec<DrmSystem>,
}

impl Encryption {
    pub(crate) const fn key_for(&self, kind: TrackKind) -> &ContentKey {
        match (&self.keys, kind) {
            (Keys::All(key), _) => key,
            (Keys::Split { video, .. }, TrackKind::Video) => video,
            (Keys::Split { audio, .. }, TrackKind::Audio) => audio,
        }
    }

    /// Each key once: split keys that are identical count as one.
    pub(crate) fn distinct_keys(&self) -> Vec<&ContentKey> {
        match &self.keys {
            Keys::All(key) => vec![key],
            Keys::Split { video, audio } if video == audio => vec![video],
            Keys::Split { video, audio } => vec![video, audio],
        }
    }

    /// Key IDs in UUID form, for status reporting: never the keys.
    pub(crate) fn key_ids(&self) -> Vec<String> {
        self.distinct_keys()
            .into_iter()
            .map(|key| uuid_string(&key.key_id))
            .collect()
    }

    /// Everything the encrypted bytes and the signalling depend on, hashed, for the URL version.
    /// Keys enter only as their own SHA-256.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        let mut feed = |part: &[u8]| {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        };
        for key in self.distinct_keys() {
            feed(&key.key_id);
            feed(&Sha256::digest(key.key.bytes()));
            feed(&key.iv);
        }
        for system in &self.systems {
            feed(&system.system_id);
            feed(system.pssh.as_deref().unwrap_or_default());
            feed(system.license_url.as_deref().unwrap_or_default().as_bytes());
            feed(system.hls_uri.as_deref().unwrap_or_default().as_bytes());
        }
        hasher.finalize().into()
    }
}

/// The wire form of `encryption` (TDD 0009, "Wire format"). Unknown fields are ignored, as
/// everywhere in the mapper contract.
#[derive(Debug, Deserialize)]
pub(crate) struct WireEncryption {
    scheme: String,
    keys: Vec<WireKey>,
    #[serde(default)]
    systems: Vec<WireSystem>,
}

#[derive(Deserialize)]
struct WireKey {
    tracks: Option<String>,
    key_id: String,
    key: String,
    iv: Option<String>,
}

/// The wire key is still a plain string, so `Debug` must not print it either.
impl fmt::Debug for WireKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireKey")
            .field("tracks", &self.tracks)
            .field("key_id", &self.key_id)
            .field("key", &"<redacted>")
            .field("iv", &self.iv)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
struct WireSystem {
    system_id: String,
    pssh: Option<String>,
    license_url: Option<String>,
    hls_uri: Option<String>,
}

impl WireEncryption {
    /// Checks every rule of TDD 0009's wire format. The reason names the field, never its value,
    /// so a rejected answer cannot leak a key into a log or an error body.
    pub(crate) fn validate(&self) -> Result<Encryption, String> {
        if self.scheme != "cbcs" {
            return Err("encryption scheme is not supported; only cbcs is".to_owned());
        }
        let keys = self.keys()?;
        let systems = self.systems()?;
        // FairPlay's key line has no key ID, so a player could not tell the audio key apart.
        if matches!(keys, Keys::Split { .. })
            && systems.iter().any(|system| system.system_id == FAIRPLAY)
        {
            return Err(
                "FairPlay needs one key for all tracks (encryption keys tracks \"all\")".to_owned(),
            );
        }
        Ok(Encryption { keys, systems })
    }

    fn keys(&self) -> Result<Keys, String> {
        let mut all = None;
        let mut video = None;
        let mut audio = None;
        for wire in &self.keys {
            let key = ContentKey {
                key_id: hex16(&wire.key_id, "key_id")?,
                key: KeyBytes(hex16(&wire.key, "key")?),
                iv: match &wire.iv {
                    Some(iv) => hex16(iv, "iv")?,
                    None => derived_iv(&hex16(&wire.key_id, "key_id")?),
                },
            };
            let slot = match wire.tracks.as_deref() {
                None | Some("all") => &mut all,
                Some("video") => &mut video,
                Some("audio") => &mut audio,
                Some(_) => {
                    return Err("encryption key tracks must be all, video, or audio".to_owned());
                }
            };
            if slot.replace(key).is_some() {
                return Err(SHAPE.to_owned());
            }
        }
        match (all, video, audio) {
            (Some(key), None, None) => Ok(Keys::All(key)),
            (None, Some(video), Some(audio)) => Ok(Keys::Split { video, audio }),
            _ => Err(SHAPE.to_owned()),
        }
    }

    fn systems(&self) -> Result<Vec<DrmSystem>, String> {
        if self.systems.len() > MAX_SYSTEMS {
            return Err(format!("encryption lists more than {MAX_SYSTEMS} systems"));
        }
        let mut systems: Vec<DrmSystem> = Vec::with_capacity(self.systems.len());
        for wire in &self.systems {
            let system_id = uuid(&wire.system_id)?;
            if systems.iter().any(|known| known.system_id == system_id) {
                return Err("encryption lists a system_id twice".to_owned());
            }
            let pssh = wire
                .pssh
                .as_deref()
                .map(|encoded| decode_pssh(encoded, &system_id))
                .transpose()?;
            let license_url = wire.license_url.as_deref().map(license_url).transpose()?;
            let hls_uri = wire.hls_uri.as_deref().map(hls_uri).transpose()?;
            if system_id == FAIRPLAY && hls_uri.is_none() {
                return Err("FairPlay needs an hls_uri".to_owned());
            }
            systems.push(DrmSystem {
                system_id,
                pssh,
                license_url,
                hls_uri,
            });
        }
        Ok(systems)
    }
}

const SHAPE: &str =
    "encryption keys must be one for all tracks, or one for video and one for audio";

pub(super) fn hex16(text: &str, field: &str) -> Result<[u8; 16], String> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return Err(format!("encryption {field} must be 32 hex digits"));
    }
    let digit = |byte: u8| (byte as char).to_digit(16);
    let mut out = [0; 16];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
            return Err(format!("encryption {field} must be 32 hex digits"));
        };
        out[index] = u8::try_from(high * 16 + low).expect("two hex digits fit in a byte");
    }
    Ok(out)
}

fn uuid(text: &str) -> Result<[u8; 16], String> {
    let groups = text.split('-').map(str::len).collect::<Vec<_>>();
    if groups != [8, 4, 4, 4, 12] {
        return Err("encryption system_id must be a UUID".to_owned());
    }
    hex16(&text.replace('-', ""), "system_id")
}

fn derived_iv(key_id: &[u8; 16]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"segmentor cbcs iv");
    hasher.update(key_id);
    hasher.finalize()[..16]
        .try_into()
        .expect("a SHA-256 digest has 16 bytes to spare")
}

fn decode_pssh(encoded: &str, system_id: &[u8; 16]) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "encryption pssh must be base64".to_owned())?;
    if bytes.len() > MAX_PSSH_BYTES {
        return Err(format!(
            "encryption pssh is larger than {MAX_PSSH_BYTES} bytes"
        ));
    }
    match parse_pssh(&bytes) {
        Some((id, _)) if id == *system_id => Ok(bytes),
        Some(_) => Err("encryption pssh names a different system_id".to_owned()),
        None => Err("encryption pssh is not a well-formed pssh box".to_owned()),
    }
}

/// A `pssh` box's system ID and data, or `None` when it is not exactly one well-formed box.
fn parse_pssh(bytes: &[u8]) -> Option<([u8; 16], &[u8])> {
    let mut reader = Reader::new(bytes);
    let size = usize::try_from(reader.u32().ok()?).ok()?;
    if size != bytes.len() || reader.take(4).ok()? != b"pssh" {
        return None;
    }
    let version = reader.full_box().ok()?;
    let system_id: [u8; 16] = reader.take(16).ok()?.try_into().ok()?;
    if version == 1 {
        let kids = usize::try_from(reader.u32().ok()?).ok()?;
        reader.take(kids.checked_mul(16)?).ok()?;
    } else if version != 0 {
        return None;
    }
    let data_size = usize::try_from(reader.u32().ok()?).ok()?;
    let data = reader.take(data_size).ok()?;
    reader.rest().is_empty().then_some((system_id, data))
}

/// The data of a `pssh` box the mapper supplied (already validated): `PlayReady`'s HLS URI needs
/// the `PlayReady` object it carries, not the whole box.
pub(crate) fn pssh_data(pssh: &[u8]) -> Option<&[u8]> {
    parse_pssh(pssh).map(|(_, data)| data)
}

fn license_url(text: &str) -> Result<String, String> {
    let url = Url::parse(text).map_err(|_| "encryption license_url is not a URL".to_owned())?;
    if url.scheme() != "https" || text.len() > MAX_URI_BYTES {
        return Err("encryption license_url must be an https URL".to_owned());
    }
    Ok(url.to_string())
}

/// Written between double quotes in an HLS attribute, so quotes and control characters would
/// end it early or corrupt the playlist.
pub(super) fn hls_uri(text: &str) -> Result<String, String> {
    if text.is_empty()
        || text.len() > MAX_URI_BYTES
        || text
            .chars()
            .any(|character| character == '"' || character.is_control())
    {
        return Err("encryption hls_uri must be 1 to 2048 characters without quotes".to_owned());
    }
    Ok(text.to_owned())
}

pub(crate) fn hex_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            write!(out, "{byte:02x}").expect("writing to a String cannot fail");
            out
        })
}

pub(crate) fn uuid_string(bytes: &[u8; 16]) -> String {
    let hex = hex_string(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KID: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff";

    fn wire(json: &str) -> WireEncryption {
        serde_json::from_str(json).expect("the test JSON is well-formed")
    }

    fn one_key(extra: &str) -> String {
        format!(r#"{{"scheme":"cbcs","keys":[{{"key_id":"{KID}","key":"{KEY}"}}]{extra}}}"#)
    }

    /// A minimal version 0 `pssh` box for `system`, with `data` as its payload.
    fn pssh(system: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let size = u32::try_from(32 + data.len()).unwrap();
        let mut boxed = size.to_be_bytes().to_vec();
        boxed.extend_from_slice(b"pssh");
        boxed.extend_from_slice(&[0, 0, 0, 0]);
        boxed.extend_from_slice(system);
        boxed.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        boxed.extend_from_slice(data);
        boxed
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn one_key_covers_every_track_and_the_iv_is_derived_from_the_key_id() {
        let encryption = wire(&one_key("")).validate().unwrap();

        let video = encryption.key_for(TrackKind::Video);
        assert_eq!(video, encryption.key_for(TrackKind::Audio));
        assert_eq!(hex_string(&video.key_id), KID);
        let expected: [u8; 16] = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"segmentor cbcs iv");
            hasher.update(video.key_id);
            hasher.finalize()[..16].try_into().unwrap()
        };
        assert_eq!(video.iv, expected);
    }

    #[test]
    fn video_and_audio_keys_are_kept_apart() {
        let json = format!(
            r#"{{"scheme":"cbcs","keys":[{{"tracks":"video","key_id":"{KID}","key":"{KEY}","iv":"{KEY}"}},{{"tracks":"audio","key_id":"{KEY}","key":"{KID}"}}]}}"#
        );

        let encryption = wire(&json).validate().unwrap();

        assert_eq!(
            hex_string(&encryption.key_for(TrackKind::Video).key_id),
            KID
        );
        assert_eq!(hex_string(&encryption.key_for(TrackKind::Video).iv), KEY);
        assert_eq!(
            hex_string(&encryption.key_for(TrackKind::Audio).key_id),
            KEY
        );
        assert_eq!(encryption.distinct_keys().len(), 2);
        assert_eq!(encryption.key_ids().len(), 2);
    }

    /// `FairPlay`'s HLS key line carries no key ID, so it cannot name a separate audio key.
    #[test]
    fn fairplay_with_split_keys_is_refused() {
        let json = format!(
            r#"{{"scheme":"cbcs","keys":[{{"tracks":"video","key_id":"{KID}","key":"{KEY}"}},{{"tracks":"audio","key_id":"{KEY}","key":"{KID}"}}],
            "systems":[{{"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2","hls_uri":"skd://asset-1"}}]}}"#
        );

        let error = wire(&json).validate().unwrap_err();

        assert!(error.contains("FairPlay"), "{error}");
        assert!(error.contains("tracks"), "names the field: {error}");
        assert!(!error.contains(KEY) && !error.contains(KID), "{error}");
    }

    #[test]
    fn malformed_encryption_is_refused_with_a_reason() {
        let cases = [
            (r#"{"scheme":"cenc","keys":[]}"#.to_owned(), "scheme"),
            (
                format!(r#"{{"scheme":"cbcs","keys":[{{"key_id":"{KID}","key":"00"}}]}}"#),
                "key",
            ),
            (
                format!(
                    r#"{{"scheme":"cbcs","keys":[{{"key_id":"zz{}","key":"{KEY}"}}]}}"#,
                    &KID[2..]
                ),
                "key_id",
            ),
            (
                format!(
                    r#"{{"scheme":"cbcs","keys":[{{"tracks":"video","key_id":"{KID}","key":"{KEY}"}}]}}"#
                ),
                "one for video and one for audio",
            ),
            (
                format!(
                    r#"{{"scheme":"cbcs","keys":[{{"tracks":"video","key_id":"{KID}","key":"{KEY}"}},{{"tracks":"video","key_id":"{KID}","key":"{KEY}"}}]}}"#
                ),
                "one for video and one for audio",
            ),
            (
                format!(
                    r#"{{"scheme":"cbcs","keys":[{{"tracks":"subtitles","key_id":"{KID}","key":"{KEY}"}}]}}"#
                ),
                "tracks",
            ),
            (
                one_key(r#","systems":[{"system_id":"not-a-uuid"}]"#),
                "system_id",
            ),
            (
                one_key(r#","systems":[{"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2"}]"#),
                "hls_uri",
            ),
            (
                one_key(&format!(
                    r#","systems":[{{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","pssh":"{}"}}]"#,
                    b64(&pssh(&PLAYREADY, b"x"))
                )),
                "pssh",
            ),
            (
                one_key(
                    r#","systems":[{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","pssh":"!!"}]"#,
                ),
                "pssh",
            ),
            (
                one_key(
                    r#","systems":[{"system_id":"e2719d58-a985-b3c9-781a-b030af78d30e","license_url":"ftp://x"}]"#,
                ),
                "license_url",
            ),
            (
                one_key(
                    r#","systems":[{"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2","hls_uri":"skd://a\"b"}]"#,
                ),
                "hls_uri",
            ),
        ];
        for (json, needle) in cases {
            let error = wire(&json).validate().expect_err(&json);
            assert!(error.contains(needle), "{json}: {error}");
            assert!(
                !error.contains(KEY),
                "a reason never echoes key material: {error}"
            );
        }
        let nine = (0..9)
            .map(|index| format!(r#"{{"system_id":"00000000-0000-0000-0000-00000000000{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        assert!(
            wire(&one_key(&format!(r#","systems":[{nine}]"#)))
                .validate()
                .unwrap_err()
                .contains('8')
        );
    }

    #[test]
    fn systems_keep_their_pssh_urls_and_uris() {
        let widevine = pssh(&WIDEVINE, b"widevine-data");
        let json = one_key(&format!(
            r#","systems":[{{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","pssh":"{}","license_url":"https://license.example.net/wv?a=1&b=2"}},{{"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2","hls_uri":"skd://asset-1"}}]"#,
            b64(&widevine)
        ));

        let encryption = wire(&json).validate().unwrap();

        assert_eq!(encryption.systems[0].system_id, WIDEVINE);
        assert_eq!(
            encryption.systems[0].pssh.as_deref(),
            Some(widevine.as_slice())
        );
        assert_eq!(pssh_data(&widevine), Some(b"widevine-data".as_slice()));
        assert_eq!(
            encryption.systems[0].license_url.as_deref(),
            Some("https://license.example.net/wv?a=1&b=2")
        );
        assert_eq!(
            encryption.systems[1].hls_uri.as_deref(),
            Some("skd://asset-1")
        );
    }

    #[test]
    fn the_fingerprint_changes_with_the_key_and_debug_never_prints_it() {
        let first = wire(&one_key("")).validate().unwrap();
        let rekeyed = wire(&one_key("").replace(KEY, "ffeeddccbbaa99887766554433221100"))
            .validate()
            .unwrap();

        assert_ne!(first.fingerprint(), rekeyed.fingerprint());
        assert_eq!(
            first.fingerprint(),
            wire(&one_key("")).validate().unwrap().fingerprint()
        );
        let printed = format!("{first:?}");
        assert!(
            !printed.contains(KEY) && !printed.contains("[0, 17, 34"),
            "{printed}"
        );
    }

    #[test]
    fn the_wire_form_never_debug_prints_the_key() {
        let printed = format!("{:?}", wire(&one_key("")));

        assert!(!printed.contains(KEY), "{printed}");
        assert!(printed.contains(KID), "{printed}");
    }

    #[test]
    fn uuids_print_in_their_canonical_form() {
        assert_eq!(
            uuid_string(&WIDEVINE),
            "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"
        );
        assert_eq!(
            uuid_string(&CLEARKEY),
            "e2719d58-a985-b3c9-781a-b030af78d30e"
        );
    }
}
