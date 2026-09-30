//! Common Encryption (ISO/IEC 23001-7) in the `cbcs` scheme, and the DRM signalling that goes with
//! it. See `docs/technical-design/0009-common-encryption-and-drm.md`.
#![allow(
    dead_code,
    unused_imports,
    reason = "TEMPORARY: wired in by later tasks of the DRM plan"
)]

mod avc;
mod bits;
mod cipher;
mod keys;
mod segment;
mod signal;

pub(crate) use avc::AvcParameters;
pub(crate) use cipher::{Cipher, Pattern};
pub(crate) use keys::{
    CLEARKEY, ContentKey, DrmSystem, Encryption, FAIRPLAY, KeyBytes, Keys, PLAYREADY, WIDEVINE,
    WireEncryption, hex_string, pssh_data, uuid_string,
};
pub(crate) use segment::{AssetProtection, PendingEncryption, TrackProtection};
pub(crate) use signal::{dash_content_protection, hls_key_lines, hls_session_keys};

#[cfg(test)]
pub(crate) mod tests_support {
    use super::{Encryption, WireEncryption};

    /// One key, with Widevine (license URL only) and `FairPlay`.
    pub(crate) fn sample_encryption() -> Encryption {
        serde_json::from_str::<WireEncryption>(
            r#"{"scheme":"cbcs","keys":[{"key_id":"0123456789abcdef0123456789abcdef","key":"00112233445566778899aabbccddeeff"}],
            "systems":[{"system_id":"edef8ba9-79d6-4ace-a3c8-27dcd51d21ed","license_url":"https://l.example.net/wv"},
                       {"system_id":"94ce86fb-07ff-4f43-adb8-93d2fa968ca2","hls_uri":"skd://a"}]}"#,
        )
        .unwrap()
        .validate()
        .unwrap()
    }
}
