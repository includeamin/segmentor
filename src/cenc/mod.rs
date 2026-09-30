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

pub(crate) use avc::AvcParameters;
pub(crate) use cipher::{Cipher, Pattern};
pub(crate) use keys::{
    CLEARKEY, ContentKey, DrmSystem, Encryption, FAIRPLAY, KeyBytes, Keys, PLAYREADY, WIDEVINE,
    WireEncryption, hex_string, pssh_data, uuid_string,
};
pub(crate) use segment::{AssetProtection, PendingEncryption, TrackProtection};
