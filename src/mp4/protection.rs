//! Reading a protected track's `sinf` and its `senc` (TDD 0014).
//!
//! `sinf` says how the track is protected: the scheme, the key ID, and where the IVs come from.
//! `senc` holds an IV and a subsample map for every sample. Only `senc` under `stbl` is read, so
//! everything here comes from `moov`, whose size `limits.max_metadata_bytes` already bounds.

use std::sync::Arc;

use super::boxes::{Reader, fourcc, invalid_media, optional_child, required_child};
use crate::error::{Error, Result};
use crate::media::{AuxTable, SourceProtection, SourceScheme, Subsample, iv16};

/// What `sinf` says about a track.
#[derive(Debug)]
pub(super) struct Sinf {
    /// The sample entry's type before protection.
    pub(super) original_format: [u8; 4],
    scheme: SourceScheme,
    key_id: [u8; 16],
    /// `default_Per_Sample_IV_Size`: 0 (a constant IV), 8, or 16.
    iv_size: usize,
    constant_iv: Option<[u8; 16]>,
    crypt_blocks: u8,
    skip_blocks: u8,
    /// `default_isProtected`: a track that is not protected is read as clear.
    is_protected: bool,
}

pub(super) fn parse_sinf(track_id: u32, sinf: &[u8]) -> Result<Sinf> {
    let unsupported = |what: String| Error::Unsupported(format!("track {track_id}: {what}"));
    let original_format: [u8; 4] = required_child(sinf, *b"frma")?
        .payload
        .get(..4)
        .ok_or_else(|| invalid_media("frma is truncated"))?
        .try_into()
        .expect("four bytes");
    // The scheme type follows the version and flags.
    let scheme_type: [u8; 4] = required_child(sinf, *b"schm")?
        .payload
        .get(4..8)
        .ok_or_else(|| invalid_media("schm is truncated"))?
        .try_into()
        .expect("four bytes");
    let scheme = SourceScheme::from_fourcc(scheme_type).ok_or_else(|| {
        unsupported(format!(
            "protection scheme `{}` is not supported",
            fourcc(scheme_type)
        ))
    })?;
    let schi = required_child(sinf, *b"schi")?;
    let tenc = required_child(schi.payload, *b"tenc")?;

    let mut reader = Reader::new(tenc.payload);
    let version = reader.full_box()?;
    reader.skip(1)?;
    let pattern = reader.u8()?;
    let is_protected = reader.u8()?;
    if is_protected > 1 {
        return Err(unsupported(format!(
            "default_isProtected of {is_protected} is not supported"
        )));
    }
    let iv_size = usize::from(reader.u8()?);
    let key_id: [u8; 16] = reader.take(16)?.try_into().expect("sixteen bytes");
    if !matches!(iv_size, 0 | 8 | 16) {
        return Err(unsupported(format!(
            "a per-sample IV of {iv_size} bytes is not supported"
        )));
    }
    let constant_iv = if is_protected == 1 && iv_size == 0 {
        let size = usize::from(reader.u8()?);
        Some(iv16(reader.take(size)?).ok_or_else(|| {
            unsupported(format!("a constant IV of {size} bytes is not supported"))
        })?)
    } else {
        None
    };
    // Only version 1 of `tenc` has a pattern; the byte is reserved in version 0.
    let (crypt_blocks, skip_blocks) = if version >= 1 {
        (pattern >> 4, pattern & 0x0f)
    } else {
        (0, 0)
    };
    Ok(Sinf {
        original_format,
        scheme,
        key_id,
        iv_size,
        constant_iv,
        crypt_blocks,
        skip_blocks,
        is_protected: is_protected == 1,
    })
}

/// The protection of a track whose sample table is `stbl` and has `sample_count` samples; `None`
/// when `tenc` says the track is not protected.
pub(super) fn read_protection(
    track_id: u32,
    sinf: &Sinf,
    stbl: &[u8],
    sample_count: usize,
) -> Result<Option<Arc<SourceProtection>>> {
    let unsupported = |what: &str| Error::Unsupported(format!("track {track_id}: {what}"));
    // Checked before the default: a clear lead is a track whose default says unprotected and
    // whose later samples a `seig` group says are protected, and reading it as clear would serve
    // them as ciphertext.
    for group in super::boxes::child_boxes(stbl)? {
        // A `roll` group (AAC priming) is harmless; one that changes the key is not read.
        if group.name == *b"sgpd" && group.payload.get(4..8) == Some(b"seig") {
            return Err(unsupported(
                "a sample group that changes the key (seig) is not supported",
            ));
        }
    }
    if !sinf.is_protected {
        return Ok(None);
    }
    let aux = match optional_child(stbl, *b"senc")? {
        Some(senc) => read_senc(track_id, senc.payload, sinf.iv_size, sample_count)?,
        None if optional_child(stbl, *b"saiz")?.is_some() => {
            return Err(unsupported(
                "sample auxiliary information without a senc box is not supported",
            ));
        }
        None if sinf.constant_iv.is_some() => AuxTable::default(),
        None => {
            return Err(unsupported(
                "a protected track needs a senc box, or a constant IV",
            ));
        }
    };
    Ok(Some(Arc::new(SourceProtection {
        scheme: sinf.scheme,
        original_format: sinf.original_format,
        key_id: sinf.key_id,
        constant_iv: sinf.constant_iv,
        crypt_blocks: sinf.crypt_blocks,
        skip_blocks: sinf.skip_blocks,
        aux,
    })))
}

/// `senc`: a sample count, then for each sample an IV of `iv_size` bytes and, when the box says
/// so (flag 2), a subsample map.
fn read_senc(
    track_id: u32,
    payload: &[u8],
    iv_size: usize,
    sample_count: usize,
) -> Result<AuxTable> {
    let mut reader = Reader::new(payload);
    let (_, flags) = reader.full_box_flags()?;
    let count = usize::try_from(reader.u32()?)
        .map_err(|_| invalid_media("senc sample count does not fit in memory"))?;
    if count != sample_count {
        return Err(Error::InvalidMedia(format!(
            "track {track_id}: senc describes {count} samples, the track has {sample_count}"
        )));
    }
    let with_subsamples = flags & 2 != 0;
    // Each sample takes at least this much, so the count can be trusted for allocation.
    let least = iv_size + if with_subsamples { 2 } else { 0 };
    if count
        .checked_mul(least)
        .is_none_or(|bytes| bytes > reader.remaining())
    {
        return Err(invalid_media("senc sample count exceeds its box"));
    }
    let mut ivs = Vec::with_capacity(count * iv_size);
    let mut sub_first = Vec::with_capacity(if with_subsamples { count + 1 } else { 0 });
    let mut subsamples = Vec::new();
    for _ in 0..count {
        ivs.extend_from_slice(reader.take(iv_size)?);
        if with_subsamples {
            sub_first.push(
                u32::try_from(subsamples.len())
                    .map_err(|_| invalid_media("senc has too many subsamples"))?,
            );
            for _ in 0..reader.u16()? {
                subsamples.push(Subsample {
                    clear: reader.u16()?,
                    protected: reader.u32()?,
                });
            }
        }
    }
    if with_subsamples {
        sub_first.push(
            u32::try_from(subsamples.len())
                .map_err(|_| invalid_media("senc has too many subsamples"))?,
        );
    }
    Ok(AuxTable::new(iv_size, ivs, sub_first, subsamples))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(name: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from(payload.len() + 8)
            .unwrap()
            .to_be_bytes()
            .to_vec();
        bytes.extend_from_slice(&name);
        bytes.extend_from_slice(payload);
        bytes
    }

    /// A `sinf` for `scheme`, with a `tenc` of the given version, pattern byte, protected flag
    /// and IV size, followed by `tail` (a constant IV, when there is one).
    fn sinf(
        scheme: [u8; 4],
        version: u8,
        pattern: u8,
        protected: u8,
        iv_size: u8,
        tail: &[u8],
    ) -> Vec<u8> {
        let mut tenc = vec![version, 0, 0, 0, 0, pattern, protected, iv_size];
        tenc.extend_from_slice(&[0x11; 16]);
        tenc.extend_from_slice(tail);
        let mut schm = vec![0; 4];
        schm.extend_from_slice(&scheme);
        schm.extend_from_slice(&[0, 1, 0, 0]);
        let mut out = boxed(*b"frma", b"avc1");
        out.extend(boxed(*b"schm", &schm));
        out.extend(boxed(*b"schi", &boxed(*b"tenc", &tenc)));
        out
    }

    #[test]
    fn tenc_version_one_carries_the_pattern_and_a_constant_iv() {
        let mut constant = vec![16];
        constant.extend_from_slice(&[0xc0; 16]);
        let parsed = parse_sinf(1, &sinf(*b"cbcs", 1, 0x19, 1, 0, &constant)).unwrap();
        assert_eq!(parsed.scheme, SourceScheme::Cbcs);
        assert_eq!(parsed.original_format, *b"avc1");
        assert_eq!((parsed.crypt_blocks, parsed.skip_blocks), (1, 9));
        assert_eq!(parsed.constant_iv, Some([0xc0; 16]));
        assert_eq!(parsed.key_id, [0x11; 16]);
        assert!(parsed.is_protected);
    }

    #[test]
    fn tenc_version_zero_has_no_pattern_and_an_eight_byte_constant_iv_is_padded() {
        let parsed = parse_sinf(1, &sinf(*b"cenc", 0, 0x19, 1, 8, &[])).unwrap();
        assert_eq!((parsed.crypt_blocks, parsed.skip_blocks), (0, 0));
        assert_eq!(parsed.iv_size, 8);
        assert_eq!(parsed.constant_iv, None);

        let mut constant = vec![8];
        constant.extend_from_slice(&[7; 8]);
        let parsed = parse_sinf(1, &sinf(*b"cbcs", 1, 0, 1, 0, &constant)).unwrap();
        let mut padded = [0; 16];
        padded[..8].fill(7);
        assert_eq!(parsed.constant_iv, Some(padded));
    }

    #[test]
    fn an_unprotected_track_is_read_as_clear() {
        let parsed = parse_sinf(1, &sinf(*b"cenc", 0, 0, 0, 8, &[])).unwrap();
        assert!(!parsed.is_protected);
        assert!(read_protection(1, &parsed, &[], 0).unwrap().is_none());
    }

    #[test]
    fn schemes_and_ivs_this_does_not_read_are_refused_by_name() {
        let error = parse_sinf(3, &sinf(*b"piff", 0, 0, 1, 8, &[]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("track 3") && error.contains("`piff`"),
            "{error}"
        );
        let error = parse_sinf(3, &sinf(*b"cenc", 0, 0, 1, 12, &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("12 bytes"), "{error}");
        let error = parse_sinf(3, &sinf(*b"cbcs", 1, 0, 1, 0, &[5, 1, 2, 3, 4, 5]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("constant IV of 5 bytes"), "{error}");
    }

    #[test]
    fn a_sinf_missing_a_part_is_refused() {
        assert!(parse_sinf(1, &boxed(*b"frma", b"avc1")).is_err());
        let truncated = sinf(*b"cenc", 0, 0, 1, 8, &[]);
        assert!(parse_sinf(1, &truncated[..truncated.len() - 10]).is_err());
    }

    /// A `senc` with `flags`, a count, and `rows` of bytes as given.
    fn senc(flags: u8, count: u32, rows: &[u8]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, flags];
        payload.extend_from_slice(&count.to_be_bytes());
        payload.extend_from_slice(rows);
        payload
    }

    #[test]
    fn senc_gives_each_sample_its_iv_and_subsamples() {
        // Two samples, 8-byte IVs: the first has two subsamples, the second none.
        let mut rows = vec![1; 8];
        rows.extend_from_slice(&[0, 2, 0, 5, 0, 0, 0, 20, 0, 6, 0, 0, 0, 30]);
        rows.extend_from_slice(&[2; 8]);
        rows.extend_from_slice(&[0, 0]);
        let table = read_senc(1, &senc(2, 2, &rows), 8, 2).unwrap();
        assert_eq!(table.iv(0), Some(&[1; 8][..]));
        assert_eq!(table.iv(1), Some(&[2; 8][..]));
        assert_eq!(
            table.subsamples(0),
            [
                Subsample {
                    clear: 5,
                    protected: 20
                },
                Subsample {
                    clear: 6,
                    protected: 30
                }
            ]
        );
        assert_eq!(table.subsamples(1), []);
        assert!(table.iv(2).is_none());
        assert_eq!(table.subsamples(2), []);
    }

    #[test]
    fn senc_without_subsamples_or_ivs_is_an_empty_table() {
        let table = read_senc(1, &senc(0, 3, &[]), 0, 3).unwrap();
        assert!(table.iv(0).is_none());
        assert_eq!(table.subsamples(0), []);
        assert_eq!(table.bytes(), 0);
    }

    #[test]
    fn senc_that_disagrees_with_the_track_or_its_own_size_is_refused() {
        let rows = [1; 8];
        let error = read_senc(4, &senc(0, 1, &rows), 8, 2)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("track 4") && error.contains("1 samples, the track has 2"),
            "{error}"
        );
        // A count far larger than the box, which must not be allocated for.
        assert!(read_senc(1, &senc(0, 1_000_000, &rows), 8, 1_000_000).is_err());
        // A subsample map cut short.
        assert!(read_senc(1, &senc(2, 1, &[1, 1, 1, 1, 1, 1, 1, 1, 0, 3, 0, 5]), 8, 1).is_err());
        assert!(read_senc(1, &senc(0, 1, &[1; 4]), 8, 1).is_err());
    }

    #[test]
    fn a_key_changing_sample_group_is_refused_and_a_roll_group_is_not() {
        let parsed = || parse_sinf(1, &sinf(*b"cenc", 0, 0, 1, 8, &[])).unwrap();
        let group = |kind: &[u8; 4]| {
            let mut payload = vec![1, 0, 0, 0];
            payload.extend_from_slice(kind);
            boxed(*b"sgpd", &payload)
        };
        let senc_box = boxed(*b"senc", &senc(0, 0, &[]));
        let mut stbl = senc_box.clone();
        stbl.extend(group(b"roll"));
        assert!(read_protection(1, &parsed(), &stbl, 0).unwrap().is_some());

        let mut stbl = senc_box;
        stbl.extend(group(b"seig"));
        let error = read_protection(1, &parsed(), &stbl, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("seig"), "{error}");
    }

    /// A clear lead: the default says unprotected, and a `seig` group marks the samples after it
    /// as protected. Reading such a track as clear would serve those samples as ciphertext.
    #[test]
    fn a_key_changing_group_is_refused_even_when_the_default_says_unprotected() {
        let parsed = parse_sinf(1, &sinf(*b"cenc", 0, 0, 0, 8, &[])).unwrap();
        let mut group = vec![1, 0, 0, 0];
        group.extend_from_slice(b"seig");
        let stbl = boxed(*b"sgpd", &group);
        let error = read_protection(1, &parsed, &stbl, 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("seig"), "{error}");
    }

    #[test]
    fn only_zero_and_one_are_values_of_default_is_protected() {
        for protected in [0, 1] {
            assert!(parse_sinf(1, &sinf(*b"cenc", 0, 0, protected, 8, &[])).is_ok());
        }
        let error = parse_sinf(2, &sinf(*b"cenc", 0, 0, 2, 8, &[]))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("track 2") && error.contains("default_isProtected"),
            "{error}"
        );
    }

    #[test]
    fn a_track_needs_a_senc_or_a_constant_iv() {
        let parsed = parse_sinf(1, &sinf(*b"cenc", 0, 0, 1, 8, &[])).unwrap();
        let error = read_protection(1, &parsed, &[], 5).unwrap_err().to_string();
        assert!(error.contains("senc"), "{error}");
        let saiz = boxed(*b"saiz", &[0; 9]);
        let parsed = parse_sinf(1, &sinf(*b"cenc", 0, 0, 1, 8, &[])).unwrap();
        let error = read_protection(1, &parsed, &saiz, 5)
            .unwrap_err()
            .to_string();
        assert!(error.contains("auxiliary information"), "{error}");
    }
}
