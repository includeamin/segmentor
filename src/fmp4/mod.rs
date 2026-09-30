mod fragment;
mod init;

#[allow(
    unused_imports,
    reason = "TEMPORARY: used by the DRM plan's later tasks"
)]
pub(crate) use fragment::encrypted_fragment_header;
pub(crate) use fragment::{PreparedSegment, prepare_media_segment, write_media_segment};
#[allow(
    unused_imports,
    reason = "TEMPORARY: used by the DRM plan's later tasks"
)]
pub(crate) use init::sample_entry;
pub(crate) use init::write_init_segment;
#[allow(
    unused_imports,
    reason = "TEMPORARY: used by the DRM plan's later tasks"
)]
pub(crate) use init::{InitProtection, write_protected_init_segment};
