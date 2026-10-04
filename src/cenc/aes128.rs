//! Whole-segment HLS `AES-128` (RFC 8216, 5.2): each media segment encrypted as one
//! AES-128-CBC message with PKCS#7 padding, and a key the player fetches from a URI. It protects
//! without a DRM system, so there are no key IDs, `pssh` boxes, or licence servers here, and it is
//! HLS only. See `docs/technical-design/0012-hls-aes-128.md`.

use std::fmt;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::cipher::{Cipher, Pattern};
use super::keys::{KeyBytes, hex16, hls_uri};

/// A validated key and the URI players fetch it from.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Aes128 {
    key: KeyBytes,
    uri: String,
}

impl fmt::Debug for Aes128 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Aes128")
            .field("key", &"<redacted>")
            .field("uri", &self.uri)
            .finish()
    }
}

impl Aes128 {
    /// Where players fetch the key: the value of the `URI` attribute of `EXT-X-KEY`.
    pub(crate) fn uri(&self) -> &str {
        &self.uri
    }

    /// Everything the encrypted bytes and the playlists depend on, hashed, for the URL version.
    /// The key enters only as its own SHA-256.
    pub(crate) fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"hls aes-128");
        hasher.update(Sha256::digest(self.key.bytes()));
        hasher.update((self.uri.len() as u64).to_be_bytes());
        hasher.update(self.uri.as_bytes());
        hasher.finalize().into()
    }

    /// The default IV of the segment with media sequence number `segment_index`: that number as
    /// a 128-bit big-endian integer (RFC 8216, 5.2). The playlists start at sequence 0 and give
    /// no `IV` attribute, so the segment's position is its IV.
    pub(crate) const fn iv(segment_index: u32) -> [u8; 16] {
        let mut iv = [0; 16];
        let bytes = segment_index.to_be_bytes();
        iv[12] = bytes[0];
        iv[13] = bytes[1];
        iv[14] = bytes[2];
        iv[15] = bytes[3];
        iv
    }

    /// Pads `data` to a whole number of blocks (PKCS#7: always at least one byte, so a multiple
    /// of 16 gains a full block) and encrypts it in place as one CBC message.
    pub(crate) fn encrypt_segment(&self, segment_index: u32, data: &mut Vec<u8>) {
        let padding = 16 - data.len() % 16;
        data.resize(
            data.len() + padding,
            u8::try_from(padding).expect("at most 16"),
        );
        Cipher::new(self.key.bytes()).encrypt_range(&Self::iv(segment_index), data, Pattern::FULL);
    }
}

/// The wire form: `{"key": "<32 hex digits>", "key_uri": "<where players fetch the key>"}`.
#[derive(Deserialize)]
pub(crate) struct WireAes128 {
    key: String,
    key_uri: String,
}

impl fmt::Debug for WireAes128 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WireAes128")
            .field("key", &"<redacted>")
            .field("key_uri", &self.key_uri)
            .finish()
    }
}

impl WireAes128 {
    /// The reason names the field, never its value, so a rejected answer cannot leak the key into
    /// a log or an error body.
    pub(crate) fn validate(&self) -> Result<Aes128, String> {
        let key = hex16(&self.key, "key")
            .map_err(|_| "hls_aes128 key must be 32 hex digits".to_owned())?;
        let uri = hls_uri(&self.key_uri).map_err(|_| {
            "hls_aes128 key_uri must be 1 to 2048 characters without quotes".to_owned()
        })?;
        Ok(Aes128 {
            key: KeyBytes::new(key),
            uri,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(json: &str) -> WireAes128 {
        serde_json::from_str(json).expect("the test JSON is well-formed")
    }

    fn sample() -> Aes128 {
        wire(
            r#"{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":"https://keys.example.com/k1"}"#,
        )
        .validate()
        .unwrap()
    }

    #[test]
    fn the_iv_is_the_media_sequence_number_as_a_big_endian_128_bit_integer() {
        assert_eq!(Aes128::iv(0), [0; 16]);
        assert_eq!(
            Aes128::iv(0x0102_0304),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn padding_always_adds_between_one_and_sixteen_bytes() {
        let aes = sample();
        for length in [0u64, 1, 15, 16, 17, 31, 32, 1000] {
            let mut data = vec![7u8; usize::try_from(length).unwrap()];
            aes.encrypt_segment(3, &mut data);
            assert_eq!(data.len() as u64, (length / 16 + 1) * 16, "{length}");
            assert_eq!(data.len() % 16, 0);
            assert!(
                data.len() as u64 > length,
                "a whole block is added to a full one"
            );
        }
    }

    /// `openssl enc -aes-128-cbc` (PKCS#7 by default) must make the same bytes from the same key
    /// and IV, for segment numbers whose IVs differ in several bytes.
    #[test]
    fn openssl_makes_the_same_ciphertext() {
        use std::io::Write;
        use std::process::{Command, Stdio};

        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("skipping: openssl is not installed");
            return;
        }
        let aes = sample();
        for (segment, length) in [
            (0u32, 0usize),
            (1, 5),
            (7, 16),
            (300, 1000),
            (0x0102_0304, 4097),
        ] {
            let clear = (0..length)
                .map(|byte| u8::try_from(byte * 7 % 251).unwrap())
                .collect::<Vec<_>>();
            let mut ours = clear.clone();
            aes.encrypt_segment(segment, &mut ours);

            let iv = u128::from_be_bytes(Aes128::iv(segment));
            let iv = format!("{iv:032x}");
            let mut child = Command::new("openssl")
                .args([
                    "enc",
                    "-aes-128-cbc",
                    "-K",
                    "2b7e151628aed2a6abf7158809cf4f3c",
                    "-iv",
                    &iv,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&clear).unwrap();
            let theirs = child.wait_with_output().unwrap().stdout;
            assert_eq!(ours, theirs, "segment {segment}, {length} bytes");
        }
    }

    #[test]
    fn the_wire_form_is_validated_without_echoing_the_key() {
        let bad_key = wire(r#"{"key":"zz","key_uri":"https://k"}"#)
            .validate()
            .unwrap_err();
        assert_eq!(bad_key, "hls_aes128 key must be 32 hex digits");
        for uri in ["", "https://a\"b", "https://a\nb"] {
            let json = format!(
                r#"{{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":{}}}"#,
                serde_json::to_string(uri).unwrap()
            );
            assert!(wire(&json).validate().is_err(), "{uri:?}");
        }
        let long = "x".repeat(2049);
        let json = format!(r#"{{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":"{long}"}}"#);
        assert!(wire(&json).validate().is_err());
        // A relative URI is fine: it resolves against the playlist.
        assert!(
            wire(r#"{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":"../keys/1"}"#)
                .validate()
                .is_ok()
        );
        assert!(!format!("{:?}", sample()).contains("2b7e"));
        assert!(
            !format!(
                "{:?}",
                wire(r#"{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":"u"}"#)
            )
            .contains("2b7e")
        );
    }

    #[test]
    fn the_fingerprint_changes_with_the_key_and_with_the_uri() {
        let base = sample().fingerprint();
        let other_key = wire(
            r#"{"key":"2b7e151628aed2a6abf7158809cf4f3d","key_uri":"https://keys.example.com/k1"}"#,
        )
        .validate()
        .unwrap();
        let other_uri = wire(
            r#"{"key":"2b7e151628aed2a6abf7158809cf4f3c","key_uri":"https://keys.example.com/k2"}"#,
        )
        .validate()
        .unwrap();
        assert_ne!(base, other_key.fingerprint());
        assert_ne!(base, other_uri.fingerprint());
        assert_eq!(base, sample().fingerprint());
    }
}
