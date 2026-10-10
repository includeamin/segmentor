//! The mapper's `source_encryption`: the keys that undo a source file's protection (TDD 0014).

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::keys::KeyBytes;
use crate::media::TrackKind;

const MAX_SOURCE_KEYS: usize = 16;

/// The wire form of `source_encryption`. Unknown fields are ignored, as everywhere in the mapper
/// contract.
#[derive(Debug, Deserialize)]
pub(crate) struct WireSourceEncryption {
    #[serde(default)]
    keys: Vec<WireSourceKey>,
    #[serde(default)]
    passthrough: bool,
}

#[derive(Deserialize)]
struct WireSourceKey {
    tracks: Option<String>,
    key_id: String,
    key: String,
}

/// The wire key is still a plain string, so `Debug` must not print it either.
impl fmt::Debug for WireSourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireSourceKey")
            .field("tracks", &self.tracks)
            .field("key_id", &self.key_id)
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceKey {
    key_id: [u8; 16],
    /// `None` is any track.
    tracks: Option<TrackKind>,
    key: KeyBytes,
}

/// Keys for a source, validated: each is matched to a track by the key ID in its `tenc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceKeys {
    keys: Vec<SourceKey>,
}

impl WireSourceEncryption {
    /// The reason names the field, never its value, so a rejected answer cannot leak a key.
    pub(crate) fn validate(&self) -> Result<SourceKeys, String> {
        if self.passthrough {
            return Err(if self.keys.is_empty() {
                "source_encryption passthrough is not supported yet".to_owned()
            } else {
                "source_encryption cannot have both keys and passthrough".to_owned()
            });
        }
        if self.keys.is_empty() {
            return Err("source_encryption needs keys".to_owned());
        }
        if self.keys.len() > MAX_SOURCE_KEYS {
            return Err(format!(
                "source_encryption lists more than {MAX_SOURCE_KEYS} keys"
            ));
        }
        let mut keys: Vec<SourceKey> = Vec::with_capacity(self.keys.len());
        for wire in &self.keys {
            let tracks = match wire.tracks.as_deref() {
                None | Some("all") => None,
                Some("video") => Some(TrackKind::Video),
                Some("audio") => Some(TrackKind::Audio),
                Some(_) => {
                    return Err(
                        "source_encryption key tracks must be all, video, or audio".to_owned()
                    );
                }
            };
            let key = SourceKey {
                key_id: hex16(&wire.key_id, "key_id")?,
                tracks,
                key: KeyBytes::new(hex16(&wire.key, "key")?),
            };
            if keys
                .iter()
                .any(|known| known.key_id == key.key_id && known.tracks == key.tracks)
            {
                return Err("source_encryption lists a key_id twice for the same tracks".to_owned());
            }
            keys.push(key);
        }
        Ok(SourceKeys { keys })
    }
}

impl SourceKeys {
    /// The key for a track of `kind` whose `tenc` names `key_id`: an entry for that kind of
    /// track before one for any track.
    pub(crate) fn key_for(&self, key_id: &[u8; 16], kind: TrackKind) -> Option<&KeyBytes> {
        let matching = |tracks: Option<TrackKind>| {
            self.keys
                .iter()
                .find(|key| key.key_id == *key_id && key.tracks == tracks)
        };
        matching(Some(kind))
            .or_else(|| matching(None))
            .map(|key| &key.key)
    }

    /// Everything the decrypted bytes depend on, hashed, for the URL version. Keys enter only as
    /// their own SHA-256, and the order the mapper listed them in does not matter.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut entries: Vec<[u8; 49]> = self
            .keys
            .iter()
            .map(|key| {
                let mut entry = [0; 49];
                entry[..16].copy_from_slice(&key.key_id);
                entry[16] = match key.tracks {
                    None => 0,
                    Some(TrackKind::Video) => 1,
                    Some(TrackKind::Audio) => 2,
                };
                entry[17..].copy_from_slice(&Sha256::digest(key.key.bytes())[..32]);
                entry
            })
            .collect();
        entries.sort_unstable();
        let mut hasher = Sha256::new();
        hasher.update(b"source keys");
        for entry in entries {
            hasher.update(entry);
        }
        hasher.finalize().into()
    }
}

fn hex16(text: &str, field: &str) -> Result<[u8; 16], String> {
    let invalid = || format!("source_encryption {field} must be 32 hex digits");
    if text.len() != 32 {
        return Err(invalid());
    }
    let mut out = [0; 16];
    for (index, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |byte: u8| (byte as char).to_digit(16);
        let (Some(high), Some(low)) = (digit(pair[0]), digit(pair[1])) else {
            return Err(invalid());
        };
        out[index] = u8::try_from(high * 16 + low).expect("two hex digits fit in a byte");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_ID: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff";
    const OTHER_ID: &str = "fedcba9876543210fedcba9876543210";

    fn wire(json: &str) -> WireSourceEncryption {
        serde_json::from_str(json).expect("the test JSON is well-formed")
    }

    fn one_key(tracks: &str) -> String {
        format!(r#"{{"keys":[{{{tracks}"key_id":"{KEY_ID}","key":"{KEY}"}}]}}"#)
    }

    fn id(hex: &str) -> [u8; 16] {
        hex16(hex, "key_id").unwrap()
    }

    #[test]
    fn a_key_is_found_by_its_key_id_and_the_kind_of_track() {
        let keys = wire(&format!(
            r#"{{"keys":[
                {{"key_id":"{KEY_ID}","key":"{KEY}"}},
                {{"tracks":"audio","key_id":"{KEY_ID}","key":"ffeeddccbbaa99887766554433221100"}},
                {{"tracks":"video","key_id":"{OTHER_ID}","key":"{KEY}"}}]}}"#
        ))
        .validate()
        .unwrap();
        let bytes = |hex: &str| hex16(hex, "key").unwrap();
        // An entry for the kind of track wins over one for any track.
        assert_eq!(
            keys.key_for(&id(KEY_ID), TrackKind::Video)
                .map(|k| *k.bytes()),
            Some(bytes(KEY))
        );
        assert_eq!(
            keys.key_for(&id(KEY_ID), TrackKind::Audio)
                .map(|k| *k.bytes()),
            Some(bytes("ffeeddccbbaa99887766554433221100"))
        );
        // A key for video does not decrypt audio, and an unknown key ID has no key.
        assert!(keys.key_for(&id(OTHER_ID), TrackKind::Audio).is_none());
        assert!(keys.key_for(&[0; 16], TrackKind::Video).is_none());
    }

    #[test]
    fn what_is_refused_is_refused_without_echoing_a_key() {
        let cases = [
            ("{}".to_owned(), "needs keys"),
            (r#"{"passthrough":true}"#.to_owned(), "not supported yet"),
            (
                format!(r#"{{"passthrough":true,"keys":[{{"key_id":"{KEY_ID}","key":"{KEY}"}}]}}"#),
                "both keys and passthrough",
            ),
            (one_key(r#""tracks":"subtitles","#), "tracks must be"),
            (
                format!(
                    r#"{{"keys":[{{"key_id":"{KEY_ID}","key":"zz112233445566778899aabbccddeeff"}}]}}"#
                ),
                "key must be 32 hex digits",
            ),
            (
                format!(r#"{{"keys":[{{"key_id":"0123","key":"{KEY}"}}]}}"#),
                "key_id must be 32 hex digits",
            ),
            (
                format!(
                    r#"{{"keys":[{{"key_id":"{KEY_ID}","key":"{KEY}"}},{{"key_id":"{KEY_ID}","key":"{KEY}"}}]}}"#
                ),
                "twice",
            ),
        ];
        for (json, expected) in cases {
            let reason = wire(&json).validate().unwrap_err();
            assert!(reason.contains(expected), "{reason}");
            assert!(!reason.contains(KEY), "{reason}");
        }
        let many = (0..=MAX_SOURCE_KEYS)
            .map(|n| format!(r#"{{"key_id":"{n:032x}","key":"{KEY}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let reason = wire(&format!(r#"{{"keys":[{many}]}}"#))
            .validate()
            .unwrap_err();
        assert!(reason.contains("more than 16"), "{reason}");
    }

    #[test]
    fn the_fingerprint_follows_the_keys_and_not_their_order_or_the_debug_output() {
        let both = |first: &str, second: &str| {
            wire(&format!(
                r#"{{"keys":[{{"key_id":"{first}","key":"{KEY}"}},{{"key_id":"{second}","key":"{KEY}"}}]}}"#
            ))
            .validate()
            .unwrap()
        };
        assert_eq!(
            both(KEY_ID, OTHER_ID).fingerprint(),
            both(OTHER_ID, KEY_ID).fingerprint()
        );
        let rekeyed = wire(&format!(
            r#"{{"keys":[{{"key_id":"{KEY_ID}","key":"ffeeddccbbaa99887766554433221100"}}]}}"#
        ))
        .validate()
        .unwrap();
        let original = wire(&one_key("")).validate().unwrap();
        assert_ne!(original.fingerprint(), rekeyed.fingerprint());
        let scoped = wire(&one_key(r#""tracks":"video","#)).validate().unwrap();
        assert_ne!(original.fingerprint(), scoped.fingerprint());
        assert!(!format!("{original:?}").contains("00112233"));
        assert!(!format!("{:?}", wire(&one_key(""))).contains("00112233"));
    }
}
