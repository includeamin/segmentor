//! What a source file says about its own encryption (TDD 0014): the scheme and key ID from
//! `sinf`, and the IV and subsample map of every sample from `senc`.

/// The four protection schemes of ISO/IEC 23001-7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceScheme {
    /// AES-CTR over every protected byte.
    Cenc,
    /// AES-CTR over the blocks a crypt/skip pattern selects.
    Cens,
    /// AES-CBC over whole blocks.
    Cbc1,
    /// AES-CBC over the blocks a crypt/skip pattern selects, from a constant IV.
    Cbcs,
}

impl SourceScheme {
    /// The scheme a `schm` box names, or `None` for anything this does not read.
    pub(crate) fn from_fourcc(code: [u8; 4]) -> Option<Self> {
        match &code {
            b"cenc" => Some(Self::Cenc),
            b"cens" => Some(Self::Cens),
            b"cbc1" => Some(Self::Cbc1),
            b"cbcs" => Some(Self::Cbcs),
            _ => None,
        }
    }
}

/// One subsample: a run of clear bytes, then a run of protected bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Subsample {
    pub(crate) clear: u16,
    pub(crate) protected: u32,
}

/// The per-sample IVs and subsample maps of one track, as flat tables.
///
/// Sample `i` is the `i`th sample of the track's tables, before any edit list or clip window
/// trims them; see `SampleIndex::window_start`. A table with no IVs and no subsample map (a
/// constant-IV track whose samples are protected whole) has nothing to look up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AuxTable {
    iv_size: usize,
    /// `iv_size` bytes per sample, or empty when the IV is constant.
    ivs: Vec<u8>,
    /// `sub_first[i]..sub_first[i + 1]` are sample `i`'s entries in `subsamples`; empty when no
    /// sample has a subsample map.
    sub_first: Vec<u32>,
    subsamples: Vec<Subsample>,
}

impl AuxTable {
    pub(crate) const fn new(
        iv_size: usize,
        ivs: Vec<u8>,
        sub_first: Vec<u32>,
        subsamples: Vec<Subsample>,
    ) -> Self {
        Self {
            iv_size,
            ivs,
            sub_first,
            subsamples,
        }
    }

    /// Sample `index`'s own IV, or `None` when the track uses a constant IV.
    pub(crate) fn iv(&self, index: usize) -> Option<&[u8]> {
        if self.iv_size == 0 {
            return None;
        }
        let start = index.checked_mul(self.iv_size)?;
        self.ivs.get(start..start.checked_add(self.iv_size)?)
    }

    /// Sample `index`'s subsamples; empty means the whole sample is protected.
    pub(crate) fn subsamples(&self, index: usize) -> &[Subsample] {
        let range = self
            .sub_first
            .get(index)
            .zip(self.sub_first.get(index.saturating_add(1)));
        match range {
            Some((&first, &end)) => self
                .subsamples
                .get(first as usize..end as usize)
                .unwrap_or_default(),
            None => &[],
        }
    }

    /// Memory held, for the index budget.
    pub(crate) fn bytes(&self) -> usize {
        self.ivs.len()
            + self.sub_first.len() * size_of::<u32>()
            + self.subsamples.len() * size_of::<Subsample>()
    }
}

/// A protected track of a source file: how it is protected and the data to undo it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceProtection {
    pub(crate) scheme: SourceScheme,
    /// The sample entry's type before protection (`frma`), such as `avc1` or `mp4a`.
    pub(crate) original_format: [u8; 4],
    /// `default_KID` from `tenc`.
    pub(crate) key_id: [u8; 16],
    /// The constant IV from `tenc`, zero-padded to 16 bytes, for a track with no per-sample IVs.
    pub(crate) constant_iv: Option<[u8; 16]>,
    pub(crate) crypt_blocks: u8,
    pub(crate) skip_blocks: u8,
    pub(crate) aux: AuxTable,
}

/// An IV of 8 or 16 bytes as the 16 bytes a cipher takes; 8-byte IVs are padded with zeros.
pub(crate) fn iv16(iv: &[u8]) -> Option<[u8; 16]> {
    match iv.len() {
        8 | 16 => {
            let mut padded = [0; 16];
            padded[..iv.len()].copy_from_slice(iv);
            Some(padded)
        }
        _ => None,
    }
}
