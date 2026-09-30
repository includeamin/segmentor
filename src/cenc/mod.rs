//! Common Encryption (ISO/IEC 23001-7) in the `cbcs` scheme, and the DRM signalling that goes with
//! it. See `docs/technical-design/0009-common-encryption-and-drm.md`.

mod avc;
mod bits;
mod cipher;
mod keys;
mod segment;
mod signal;

pub(crate) use keys::{Encryption, WireEncryption};
pub(crate) use segment::{AssetProtection, PendingEncryption};
pub(crate) use signal::{dash_content_protection, hls_key_lines, hls_session_keys};

/// See `fuzzing::exercise_avc_slice_header`.
pub(crate) fn fuzz_avc(data: &[u8]) {
    let Some((&split, rest)) = data.split_first() else {
        return;
    };
    let split = usize::from(split).min(rest.len());
    let (sps, rest) = rest.split_at(split);
    let (pps, slice) = rest.split_at(rest.len() / 2);
    let mut parameters = avc::AvcParameters::empty(4);
    let _ = parameters.update(sps);
    let _ = parameters.update(pps);
    let _ = parameters.clear_bytes(slice);
    let _ = segment::avc_subsamples(&parameters, slice);
}

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
