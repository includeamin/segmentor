//! Just enough H.265 (ITU-T H.265, 7.3) to find where each slice segment header ends, so `cbcs`
//! can leave it clear (ISO/IEC 23001-7, 10.4). Every read is bounded by the NAL unit; anything this
//! does not understand fails closed.
//!
//! A slice segment header depends on the SPS and PPS it names, down to the reference picture sets,
//! so those are parsed far enough to answer the questions the header asks of them.

use std::collections::HashMap;

use super::bits::{BitReader, Rbsp};
use crate::error::{Error, Result};
use crate::mp4::boxes::{Reader, optional_child};
use crate::mp4::codec::visual_entry;

/// Most slice headers are a few dozen bytes, so parsing starts with this much of the NAL unit; one
/// with many entry points (wavefronts or tiles) is parsed again with [`LARGE_SLICE_HEADER_BYTES`].
const SLICE_HEADER_BYTES: usize = 1024;
const LARGE_SLICE_HEADER_BYTES: usize = 64 * 1024;
const MAX_PARAMETER_SET_BYTES: usize = 64 * 1024;
const MAX_REFERENCE_SETS: u32 = 64;
const MAX_REFERENCES_PER_SET: u32 = 16;
const MAX_LONG_TERM_PICTURES: u32 = 32;

const NAL_SPS: u8 = 33;
const NAL_PPS: u8 = 34;

/// One short-term reference picture set (H.265 7.3.7, 7.4.8): the pictures before and after the
/// current one that it keeps, and whether each is used for the current picture's prediction.
#[derive(Debug, Clone, Default)]
struct ReferenceSet {
    /// Negative POC deltas, nearest first, with `used_by_curr_pic`.
    before: Vec<(i32, bool)>,
    /// Positive POC deltas, nearest first, with `used_by_curr_pic`.
    after: Vec<(i32, bool)>,
}

impl ReferenceSet {
    fn len(&self) -> usize {
        self.before.len() + self.after.len()
    }

    fn used(&self) -> usize {
        self.before
            .iter()
            .chain(&self.after)
            .filter(|(_, used)| *used)
            .count()
    }
}

/// The SPS flags are the syntax elements themselves (H.265 7.3.2.2), not a state machine.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the SPS syntax elements"
)]
#[derive(Debug, Clone)]
struct Sps {
    chroma_array_type: u32,
    separate_colour_plane: bool,
    /// `PicSizeInCtbsY`, which sets the width of `slice_segment_address`.
    pic_size_in_ctbs: u64,
    log2_max_poc_lsb: usize,
    sample_adaptive_offset: bool,
    reference_sets: Vec<ReferenceSet>,
    long_term_present: bool,
    /// `used_by_curr_pic_lt_sps_flag` of each long-term picture the SPS lists.
    long_term_used: Vec<bool>,
    temporal_mvp: bool,
}

/// The PPS flags are the syntax elements themselves (H.265 7.3.2.3), not a state machine.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the PPS syntax elements"
)]
#[derive(Debug, Clone)]
pub(super) struct Pps {
    sps_id: u32,
    dependent_slice_segments: bool,
    output_flag_present: bool,
    extra_slice_header_bits: usize,
    cabac_init_present: bool,
    num_ref_idx_l0_default: u32,
    num_ref_idx_l1_default: u32,
    weighted_pred: bool,
    weighted_bipred: bool,
    slice_chroma_qp_offsets_present: bool,
    chroma_qp_offset_list_enabled: bool,
    deblocking_override_enabled: bool,
    deblocking_disabled: bool,
    loop_filter_across_slices: bool,
    lists_modification_present: bool,
    entry_points: bool,
    header_extension_present: bool,
}

/// A track's parameter sets and NAL length size, from `hvcC`, updated by in-band sets.
#[derive(Debug, Clone)]
pub(crate) struct HevcParameters {
    nal_length_size: usize,
    sps: HashMap<u32, Sps>,
    pub(super) pps: HashMap<u32, Pps>,
}

impl HevcParameters {
    /// No parameter sets yet (for the fuzz entry point).
    pub(crate) fn empty(nal_length_size: usize) -> Self {
        Self {
            nal_length_size,
            sps: HashMap::new(),
            pps: HashMap::new(),
        }
    }

    /// Reads `hvcC` from a visual sample entry's payload.
    pub(crate) fn from_sample_entry(entry: &[u8]) -> Result<Self> {
        let (_, _, children) = visual_entry(entry)?;
        let config = optional_child(children, *b"hvcC")?
            .ok_or_else(|| Error::Unsupported("H.265 without hvcC".to_owned()))?;
        let mut reader = Reader::new(config.payload);
        // Version, profile, compatibility, constraint flags, level, segmentation, parallelism,
        // chroma format, bit depths, and frame rate.
        reader.skip(21)?;
        let mut parameters = Self::empty(usize::from(reader.u8()? & 3) + 1);
        for _ in 0..reader.u8()? {
            reader.skip(1)?;
            for _ in 0..reader.u16()? {
                let length = usize::from(reader.u16()?);
                parameters.update(reader.take(length)?)?;
            }
        }
        Ok(parameters)
    }

    pub(crate) const fn nal_length_size(&self) -> usize {
        self.nal_length_size
    }

    /// Takes in a VPS, SPS, or PPS NAL unit (header included); other NAL units are ignored.
    pub(crate) fn update(&mut self, nal: &[u8]) -> Result<()> {
        match nal.first().map(|header| (header >> 1) & 0x3f) {
            Some(NAL_SPS) => {
                let (id, sps) = parse_sps(nal)?;
                self.sps.insert(id, sps);
            }
            Some(NAL_PPS) => {
                let (id, pps) = parse_pps(nal)?;
                self.pps.insert(id, pps);
            }
            _ => {}
        }
        Ok(())
    }

    /// How many bytes of this slice segment NAL unit (header included) must stay clear.
    pub(crate) fn clear_bytes(&self, nal: &[u8]) -> Result<usize> {
        let attempt = |limit: usize| {
            let rbsp = Rbsp::new(nal, limit);
            let bits = self.header_bits(&rbsp)?;
            rbsp.nal_bytes_covering(bits)
                .ok_or_else(|| Error::InvalidMedia("a slice header is truncated".to_owned()))
        };
        match attempt(SLICE_HEADER_BYTES) {
            Err(_) if nal.len() > SLICE_HEADER_BYTES => attempt(LARGE_SLICE_HEADER_BYTES),
            other => other,
        }
    }

    /// The slice segment header's length in RBSP bits, NAL header and `byte_alignment()`
    /// included (H.265 7.3.6.1).
    pub(super) fn header_bits(&self, rbsp: &Rbsp) -> Result<usize> {
        let mut r = rbsp.reader();
        r.skip(1)?;
        let nal_type = r.bits(6)?;
        let layer_id = r.bits(6)?;
        r.skip(3)?;
        if !matches!(nal_type, 0..=9 | 16..=21) {
            return Err(Error::Unsupported(format!(
                "H.265 NAL unit type {nal_type} cannot be encrypted"
            )));
        }
        if layer_id != 0 {
            return Err(Error::Unsupported(
                "H.265 layers above the base layer cannot be encrypted".to_owned(),
            ));
        }
        let irap = (16..=23).contains(&nal_type);
        let idr = matches!(nal_type, 19 | 20);

        let first_slice_segment = r.bit()?;
        if irap {
            r.skip(1)?; // no_output_of_prior_pics_flag
        }
        let pps = self
            .pps
            .get(&r.ue()?)
            .ok_or_else(|| Error::InvalidMedia("a slice names an unknown PPS".to_owned()))?;
        let sps = self
            .sps
            .get(&pps.sps_id)
            .ok_or_else(|| Error::InvalidMedia("a PPS names an unknown SPS".to_owned()))?;
        let mut dependent = false;
        if !first_slice_segment {
            if pps.dependent_slice_segments {
                dependent = r.bit()?;
            }
            r.skip(ceil_log2(sps.pic_size_in_ctbs))?; // slice_segment_address
        }
        if !dependent {
            slice_fields(&mut r, sps, pps, idr)?;
        }
        if pps.entry_points {
            let count = r.ue()?;
            if count > 0 {
                let width = r.ue()?.checked_add(1).filter(|width| *width <= 32);
                let width = width.ok_or_else(|| {
                    Error::InvalidMedia("an H.265 entry point width is out of range".to_owned())
                })?;
                for _ in 0..count {
                    r.skip(width as usize)?;
                }
            }
        }
        if pps.header_extension_present {
            let length = r.ue()?;
            r.skip((length as usize).saturating_mul(8))?;
        }
        // byte_alignment(): a one bit, then zero bits up to the byte boundary.
        Ok((r.position() / 8 + 1) * 8)
    }
}

/// The fields of a slice segment header that an independent (not dependent) slice segment has.
#[allow(
    clippy::too_many_lines,
    reason = "one function per syntax structure keeps this readable against H.265 7.3.6.1"
)]
fn slice_fields(r: &mut BitReader<'_>, sps: &Sps, pps: &Pps, idr: bool) -> Result<()> {
    r.skip(pps.extra_slice_header_bits)?;
    let slice_type = r.ue()?;
    if slice_type > 2 {
        return Err(Error::InvalidMedia(
            "an H.265 slice type is out of range".to_owned(),
        ));
    }
    let (b, p) = (slice_type == 0, slice_type == 1);
    if pps.output_flag_present {
        r.skip(1)?;
    }
    if sps.separate_colour_plane {
        r.skip(2)?;
    }
    let mut temporal_mvp = false;
    let mut pictures_total = 0usize;
    if !idr {
        r.skip(sps.log2_max_poc_lsb)?;
        let from_sps = r.bit()?;
        let owned;
        let current_set = if from_sps {
            let count = sps.reference_sets.len();
            let index = if count > 1 {
                r.bits(ceil_log2(count as u64))? as usize
            } else {
                0
            };
            sps.reference_sets.get(index).ok_or_else(|| {
                Error::InvalidMedia("a slice names a missing reference picture set".to_owned())
            })?
        } else {
            owned = reference_set(r, sps.reference_sets.len(), &sps.reference_sets)?;
            &owned
        };
        pictures_total += current_set.used();
        if sps.long_term_present {
            let in_sps = sps.long_term_used.len();
            let from_sps_count = if in_sps > 0 { r.ue()? } else { 0 };
            let explicit = r.ue()?;
            let total = from_sps_count
                .checked_add(explicit)
                .filter(|total| *total <= MAX_LONG_TERM_PICTURES)
                .ok_or_else(|| {
                    Error::InvalidMedia("too many H.265 long-term pictures".to_owned())
                })?;
            for index in 0..total {
                if index < from_sps_count {
                    let which = if in_sps > 1 {
                        r.bits(ceil_log2(in_sps as u64))? as usize
                    } else {
                        0
                    };
                    if sps.long_term_used.get(which).copied().unwrap_or(false) {
                        pictures_total += 1;
                    }
                } else {
                    r.skip(sps.log2_max_poc_lsb)?;
                    if r.bit()? {
                        pictures_total += 1;
                    }
                }
                if r.bit()? {
                    r.ue()?; // delta_poc_msb_cycle_lt
                }
            }
        }
        if sps.temporal_mvp {
            temporal_mvp = r.bit()?;
        }
    }
    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sample_adaptive_offset {
        sao_luma = r.bit()?;
        if sps.chroma_array_type != 0 {
            sao_chroma = r.bit()?;
        }
    }
    if b || p {
        let (mut l0, mut l1) = (pps.num_ref_idx_l0_default, pps.num_ref_idx_l1_default);
        if r.bit()? {
            l0 = ref_count(r)?;
            if b {
                l1 = ref_count(r)?;
            }
        }
        if pps.lists_modification_present && pictures_total > 1 {
            let width = ceil_log2(pictures_total as u64);
            list_modification(r, l0, width)?;
            if b {
                list_modification(r, l1, width)?;
            }
        }
        if b {
            r.skip(1)?; // mvd_l1_zero_flag
        }
        if pps.cabac_init_present {
            r.skip(1)?;
        }
        if temporal_mvp {
            let from_l0 = if b { r.bit()? } else { true };
            if (from_l0 && l0 > 1) || (!from_l0 && l1 > 1) {
                r.ue()?; // collocated_ref_idx
            }
        }
        if (pps.weighted_pred && p) || (pps.weighted_bipred && b) {
            pred_weight_table(r, sps.chroma_array_type, l0, if b { l1 } else { 0 })?;
        }
        r.ue()?; // five_minus_max_num_merge_cand
    }
    r.se()?; // slice_qp_delta
    if pps.slice_chroma_qp_offsets_present {
        r.se()?;
        r.se()?;
    }
    if pps.chroma_qp_offset_list_enabled {
        r.skip(1)?; // cu_chroma_qp_offset_enabled_flag
    }
    let override_deblocking = pps.deblocking_override_enabled && r.bit()?;
    let mut deblocking_disabled = pps.deblocking_disabled;
    if override_deblocking {
        deblocking_disabled = r.bit()?;
        if !deblocking_disabled {
            r.se()?;
            r.se()?;
        }
    }
    if pps.loop_filter_across_slices && (sao_luma || sao_chroma || !deblocking_disabled) {
        r.skip(1)?;
    }
    Ok(())
}

fn ref_count(r: &mut BitReader<'_>) -> Result<u32> {
    let count = r.ue()?.checked_add(1).filter(|count| *count <= 16);
    count.ok_or_else(|| Error::InvalidMedia("too many H.265 reference indices".to_owned()))
}

/// `ref_pic_lists_modification` for one list: a flag, then an entry per active reference.
fn list_modification(r: &mut BitReader<'_>, active: u32, width: usize) -> Result<()> {
    if r.bit()? {
        for _ in 0..active {
            r.skip(width)?;
        }
    }
    Ok(())
}

fn pred_weight_table(r: &mut BitReader<'_>, chroma: u32, l0: u32, l1: u32) -> Result<()> {
    r.ue()?; // luma_log2_weight_denom
    if chroma != 0 {
        r.se()?; // delta_chroma_log2_weight_denom
    }
    for count in [l0, l1] {
        let mut luma = Vec::with_capacity(count as usize);
        for _ in 0..count {
            luma.push(r.bit()?);
        }
        let mut chroma_flags = Vec::with_capacity(count as usize);
        if chroma != 0 {
            for _ in 0..count {
                chroma_flags.push(r.bit()?);
            }
        }
        for (index, &weighted) in luma.iter().enumerate() {
            if weighted {
                r.se()?;
                r.se()?;
            }
            if chroma_flags.get(index).copied().unwrap_or(false) {
                for _ in 0..2 {
                    r.se()?;
                    r.se()?;
                }
            }
        }
    }
    Ok(())
}

fn ceil_log2(value: u64) -> usize {
    if value <= 1 {
        0
    } else {
        (u64::BITS - (value - 1).leading_zeros()) as usize
    }
}

/// `st_ref_pic_set(index)` (H.265 7.3.7). `sets` holds the sets before `index`; in a slice header
/// `index` equals `count`, and an inter-predicted set names which earlier set it builds on.
fn reference_set(
    r: &mut BitReader<'_>,
    count: usize,
    sets: &[ReferenceSet],
) -> Result<ReferenceSet> {
    set_at(r, count, count, sets)
}

fn set_at(
    r: &mut BitReader<'_>,
    index: usize,
    count: usize,
    sets: &[ReferenceSet],
) -> Result<ReferenceSet> {
    let inter = index != 0 && r.bit()?;
    if !inter {
        let negative = bounded(r.ue()?, MAX_REFERENCES_PER_SET)?;
        let positive = bounded(r.ue()?, MAX_REFERENCES_PER_SET)?;
        let mut set = ReferenceSet::default();
        let mut poc = 0i32;
        for _ in 0..negative {
            poc = poc.saturating_sub(
                i32::try_from(r.ue()?)
                    .map_err(|_| range())?
                    .saturating_add(1),
            );
            set.before.push((poc, r.bit()?));
        }
        poc = 0;
        for _ in 0..positive {
            poc = poc.saturating_add(
                i32::try_from(r.ue()?)
                    .map_err(|_| range())?
                    .saturating_add(1),
            );
            set.after.push((poc, r.bit()?));
        }
        return Ok(set);
    }
    let delta_index = if index == count {
        usize::try_from(r.ue()?)
            .map_err(|_| range())?
            .saturating_add(1)
    } else {
        1
    };
    let reference = index
        .checked_sub(delta_index)
        .and_then(|at| sets.get(at))
        .ok_or_else(|| {
            Error::InvalidMedia("an H.265 reference set names a missing set".to_owned())
        })?;
    let negative_sign = r.bit()?;
    let magnitude = i32::try_from(r.ue()?)
        .map_err(|_| range())?
        .saturating_add(1);
    let delta_rps = if negative_sign { -magnitude } else { magnitude };
    let entries = reference.len();
    let (mut used, mut keep) = (Vec::new(), Vec::new());
    for _ in 0..=entries {
        let flag = r.bit()?;
        used.push(flag);
        keep.push(flag || r.bit()?);
    }
    // H.265 7-61 and 7-62.
    let negative_count = reference.before.len();
    let mut set = ReferenceSet::default();
    for (offset, &(poc, _)) in reference.after.iter().enumerate().rev() {
        let moved = poc.saturating_add(delta_rps);
        if moved < 0 && keep[negative_count + offset] {
            set.before.push((moved, used[negative_count + offset]));
        }
    }
    if delta_rps < 0 && keep[entries] {
        set.before.push((delta_rps, used[entries]));
    }
    for (offset, &(poc, _)) in reference.before.iter().enumerate() {
        let moved = poc.saturating_add(delta_rps);
        if moved < 0 && keep[offset] {
            set.before.push((moved, used[offset]));
        }
    }
    for (offset, &(poc, _)) in reference.before.iter().enumerate().rev() {
        let moved = poc.saturating_add(delta_rps);
        if moved > 0 && keep[offset] {
            set.after.push((moved, used[offset]));
        }
    }
    if delta_rps > 0 && keep[entries] {
        set.after.push((delta_rps, used[entries]));
    }
    for (offset, &(poc, _)) in reference.after.iter().enumerate() {
        let moved = poc.saturating_add(delta_rps);
        if moved > 0 && keep[negative_count + offset] {
            set.after.push((moved, used[negative_count + offset]));
        }
    }
    Ok(set)
}

fn bounded(value: u32, limit: u32) -> Result<u32> {
    if value > limit {
        return Err(Error::InvalidMedia(
            "an H.265 reference picture set is too large".to_owned(),
        ));
    }
    Ok(value)
}

fn range() -> Error {
    Error::InvalidMedia("an H.265 value is out of range".to_owned())
}

/// `profile_tier_level(1, max_sub_layers_minus1)` (H.265 7.3.3), read for its profile.
fn profile_tier_level(r: &mut BitReader<'_>, max_sub_layers_minus1: u32) -> Result<()> {
    r.skip(3)?; // general_profile_space and general_tier_flag
    let profile = r.bits(5)?;
    // Multiview, scalable, 3D, and screen-content profiles add slice header fields that this
    // does not read, so a stream in one is refused rather than encrypted wrongly.
    if (6..=11).contains(&profile) {
        return Err(Error::Unsupported(format!(
            "H.265 profile {profile} cannot be encrypted"
        )));
    }
    r.skip(32 + 4 + 43 + 1 + 8)?;
    let mut profile_present = Vec::new();
    let mut level_present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        profile_present.push(r.bit()?);
        level_present.push(r.bit()?);
    }
    if max_sub_layers_minus1 > 0 {
        r.skip(2 * (8 - max_sub_layers_minus1 as usize))?;
    }
    for (profile, level) in profile_present.into_iter().zip(level_present) {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    Ok(())
}

/// `scaling_list_data()` (H.265 7.3.4), skipped.
fn scaling_list_data(r: &mut BitReader<'_>) -> Result<()> {
    for size_id in 0..4u32 {
        let step = if size_id == 3 { 3 } else { 1 };
        for _ in (0..6).step_by(step) {
            if r.bit()? {
                let coefficients = 64.min(1usize << (4 + (size_id << 1)));
                if size_id > 1 {
                    r.se()?;
                }
                for _ in 0..coefficients {
                    r.se()?;
                }
            } else {
                r.ue()?;
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one function per syntax structure keeps this readable against H.265 7.3.2.2"
)]
fn parse_sps(nal: &[u8]) -> Result<(u32, Sps)> {
    let rbsp = Rbsp::new(nal, MAX_PARAMETER_SET_BYTES);
    let mut r = rbsp.reader();
    r.skip(16)?;
    r.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = r.bits(3)?;
    r.skip(1)?;
    profile_tier_level(&mut r, max_sub_layers_minus1)?;
    let id = r.ue()?;
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc > 3 {
        return Err(Error::InvalidMedia(
            "an H.265 chroma format is out of range".to_owned(),
        ));
    }
    let separate_colour_plane = chroma_format_idc == 3 && r.bit()?;
    let width = u64::from(r.ue()?);
    let height = u64::from(r.ue()?);
    if r.bit()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?; // bit_depth_luma_minus8
    r.ue()?; // bit_depth_chroma_minus8
    let log2_max_poc_lsb = r.ue()?.checked_add(4).filter(|bits| *bits <= 16);
    let log2_max_poc_lsb = log2_max_poc_lsb.ok_or_else(range)? as usize;
    let ordering_info_present = r.bit()?;
    let first_layer = if ordering_info_present {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first_layer..=max_sub_layers_minus1 {
        for _ in 0..3 {
            r.ue()?;
        }
    }
    let log2_min_cb = r.ue()?.checked_add(3).filter(|bits| *bits <= 6);
    let log2_min_cb = log2_min_cb.ok_or_else(range)?;
    let log2_diff_cb = r.ue()?;
    let log2_ctb = log2_min_cb
        .checked_add(log2_diff_cb)
        .filter(|bits| *bits <= 6)
        .ok_or_else(range)?;
    r.ue()?; // log2_min_luma_transform_block_size_minus2
    r.ue()?; // log2_diff_max_min_luma_transform_block_size
    r.ue()?; // max_transform_hierarchy_depth_inter
    r.ue()?; // max_transform_hierarchy_depth_intra
    if r.bit()? && r.bit()? {
        scaling_list_data(&mut r)?;
    }
    r.skip(1)?; // amp_enabled_flag
    let sample_adaptive_offset = r.bit()?;
    if r.bit()? {
        r.skip(8)?;
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    let reference_set_count = bounded(r.ue()?, MAX_REFERENCE_SETS)? as usize;
    let mut reference_sets: Vec<ReferenceSet> = Vec::with_capacity(reference_set_count);
    for index in 0..reference_set_count {
        let set = set_at(&mut r, index, reference_set_count, &reference_sets)?;
        reference_sets.push(set);
    }
    let long_term_present = r.bit()?;
    let mut long_term_used = Vec::new();
    if long_term_present {
        let count = bounded(r.ue()?, MAX_LONG_TERM_PICTURES)?;
        for _ in 0..count {
            r.skip(log2_max_poc_lsb)?;
            long_term_used.push(r.bit()?);
        }
    }
    let temporal_mvp = r.bit()?;

    let ctb = 1u64 << log2_ctb;
    let pic_size_in_ctbs = width.div_ceil(ctb) * height.div_ceil(ctb);
    Ok((
        id,
        Sps {
            chroma_array_type: if separate_colour_plane {
                0
            } else {
                chroma_format_idc
            },
            separate_colour_plane,
            pic_size_in_ctbs,
            log2_max_poc_lsb,
            sample_adaptive_offset,
            reference_sets,
            long_term_present,
            long_term_used,
            temporal_mvp,
        },
    ))
}

fn parse_pps(nal: &[u8]) -> Result<(u32, Pps)> {
    let rbsp = Rbsp::new(nal, MAX_PARAMETER_SET_BYTES);
    let mut r = rbsp.reader();
    r.skip(16)?;
    let id = r.ue()?;
    let sps_id = r.ue()?;
    let dependent_slice_segments = r.bit()?;
    let output_flag_present = r.bit()?;
    let extra_slice_header_bits = r.bits(3)? as usize;
    r.skip(1)?; // sign_data_hiding_enabled_flag
    let cabac_init_present = r.bit()?;
    let num_ref_idx_l0_default = ref_count(&mut r)?;
    let num_ref_idx_l1_default = ref_count(&mut r)?;
    r.se()?; // init_qp_minus26
    r.skip(1)?; // constrained_intra_pred_flag
    let transform_skip = r.bit()?;
    if r.bit()? {
        r.ue()?; // diff_cu_qp_delta_depth
    }
    r.se()?;
    r.se()?;
    let slice_chroma_qp_offsets_present = r.bit()?;
    let weighted_pred = r.bit()?;
    let weighted_bipred = r.bit()?;
    r.skip(1)?; // transquant_bypass_enabled_flag
    let tiles = r.bit()?;
    let wavefronts = r.bit()?;
    if tiles {
        let columns = bounded(r.ue()?, 255)?;
        let rows = bounded(r.ue()?, 255)?;
        if !r.bit()? {
            for _ in 0..columns + rows {
                r.ue()?;
            }
        }
        r.skip(1)?; // loop_filter_across_tiles_enabled_flag
    }
    let loop_filter_across_slices = r.bit()?;
    let (mut deblocking_override_enabled, mut deblocking_disabled) = (false, false);
    if r.bit()? {
        deblocking_override_enabled = r.bit()?;
        deblocking_disabled = r.bit()?;
        if !deblocking_disabled {
            r.se()?;
            r.se()?;
        }
    }
    if r.bit()? {
        scaling_list_data(&mut r)?;
    }
    let lists_modification_present = r.bit()?;
    r.ue()?; // log2_parallel_merge_level_minus2
    let header_extension_present = r.bit()?;
    let mut chroma_qp_offset_list_enabled = false;
    if r.bit()? {
        let range_extension = r.bit()?;
        let multilayer = r.bit()?;
        let three_d = r.bit()?;
        let screen_content = r.bit()?;
        r.skip(4)?;
        if multilayer || three_d || screen_content {
            return Err(Error::Unsupported(
                "H.265 multilayer, 3D, and screen-content extensions cannot be encrypted"
                    .to_owned(),
            ));
        }
        if range_extension {
            if transform_skip {
                r.ue()?; // log2_max_transform_skip_block_size_minus2
            }
            r.skip(1)?; // cross_component_prediction_enabled_flag
            chroma_qp_offset_list_enabled = r.bit()?;
        }
    }
    Ok((
        id,
        Pps {
            sps_id,
            dependent_slice_segments,
            output_flag_present,
            extra_slice_header_bits,
            cabac_init_present,
            num_ref_idx_l0_default,
            num_ref_idx_l1_default,
            weighted_pred,
            weighted_bipred,
            slice_chroma_qp_offsets_present,
            chroma_qp_offset_list_enabled,
            deblocking_override_enabled,
            deblocking_disabled,
            loop_filter_across_slices,
            lists_modification_present,
            entry_points: tiles || wavefronts,
            header_extension_present,
        },
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;

    use super::*;
    use crate::config::LimitsConfig;
    use crate::media::TrackKind;
    use crate::source::{ByteRange, LocalMediaSource, MediaSourceKind};

    /// The stream's parameters and every video sample's bytes.
    pub(crate) fn samples(path: &Path) -> (HevcParameters, Vec<Vec<u8>>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let source = MediaSourceKind::Local(Arc::new(LocalMediaSource::open(path).unwrap()));
            let parsed = crate::mp4::parse(&source, &LimitsConfig::default())
                .await
                .unwrap();
            let video = parsed
                .index
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap();
            let (_, entry) = crate::fmp4::sample_entry(&parsed.metadata, video.id).unwrap();
            let parameters = HevcParameters::from_sample_entry(&entry).unwrap();
            let mut bytes = Vec::new();
            for sample in &video.samples {
                let range = ByteRange::new(sample.offset, u64::from(sample.size));
                bytes.push(source.read_range(range).await.unwrap().to_vec());
            }
            (parameters, bytes)
        })
    }

    fn nal_units(sample: &[u8], length_size: usize) -> Vec<&[u8]> {
        let mut units = Vec::new();
        let mut at = 0;
        while at < sample.len() {
            let length = sample[at..at + length_size]
                .iter()
                .fold(0usize, |value, byte| value << 8 | usize::from(*byte));
            units.push(&sample[at + length_size..at + length_size + length]);
            at += length_size + length;
        }
        units
    }

    /// Where `FFmpeg`'s own parser says each slice segment header ends, in bits: the position of
    /// `alignment_bit_equal_to_one` in `trace_headers`' output, rounded up to the byte it ends.
    pub(super) fn ffmpeg_header_ends(input: &[&str]) -> Option<Vec<usize>> {
        let output = Command::new("ffmpeg")
            .args(["-v", "trace", "-nostdin"])
            .args(input)
            .args([
                "-map",
                "0:v:0",
                "-c",
                "copy",
                "-bsf:v",
                "trace_headers",
                "-f",
                "null",
                "-",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stderr);
        let mut in_slice = false;
        let mut ends = Vec::new();
        for line in text.lines() {
            // The demuxer logs its own lines in between; only the filter's count.
            if !line.starts_with("[trace_headers") {
                continue;
            }
            let Some((_, rest)) = line.split_once("] ") else {
                continue;
            };
            let mut words = rest.split_whitespace();
            let first = words.next().unwrap_or_default();
            if let Ok(position) = first.parse::<usize>() {
                if in_slice && words.next() == Some("alignment_bit_equal_to_one") {
                    ends.push((position / 8 + 1) * 8);
                    in_slice = false;
                }
            } else {
                in_slice = rest.starts_with("Slice Segment Header");
            }
        }
        Some(ends)
    }

    pub(super) fn ffmpeg_available() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// The NAL units of an Annex B stream with four-byte start codes, escapes left in.
    pub(super) fn nal_units_annex_b(stream: &[u8]) -> Vec<Vec<u8>> {
        let starts = stream
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == [0, 0, 0, 1])
            .map(|(at, _)| at)
            .collect::<Vec<_>>();
        starts
            .iter()
            .enumerate()
            .map(|(index, &at)| {
                let end = starts.get(index + 1).copied().unwrap_or(stream.len());
                stream[at + 4..end].to_vec()
            })
            .collect()
    }

    fn encoder_available() -> bool {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-encoders"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("libx265"))
    }

    /// A short x265 stream with the given settings, made once and kept under `target/`.
    ///
    /// Tests run in parallel and each wants the same streams, so one at a time makes them: the
    /// others then find the finished file instead of racing to write it.
    fn encode(name: &str, filters: &str, pixel_format: &str, tag: &str, params: &str) -> PathBuf {
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/hevc-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{name}.mp4"));
        if !path.exists() {
            let partial = directory.join(format!("{name}.{}.partial.mp4", std::process::id()));
            let status = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-nostdin", "-f", "lavfi", "-i"])
                .arg("testsrc2=size=320x180:rate=15:duration=3")
                .args(["-vf", filters, "-c:v", "libx265", "-pix_fmt", pixel_format])
                .args(["-tag:v", tag, "-x265-params"])
                .arg(format!("log-level=error:{params}"))
                .args(["-an", "-movflags", "+faststart"])
                .arg(&partial)
                .status()
                .expect("ffmpeg runs");
            assert!(status.success(), "encoding {name}");
            std::fs::rename(&partial, &path).unwrap();
        }
        path
    }

    /// Streams covering what encoders do to a slice header: reference pictures and B-frames,
    /// several slices, wavefront entry points, weighted prediction, other bit depths and chroma
    /// formats, and parameter sets carried in the samples.
    fn variants() -> Vec<(&'static str, PathBuf)> {
        let plain = "null";
        let fade = "fade=t=in:st=0:d=2";
        [
            (
                "medium",
                plain,
                "yuv420p",
                "hvc1",
                "keyint=15:bframes=4:b-pyramid=1",
            ),
            // Six references: x265 3.5, which distribution packages ship, refuses more at this
            // size, though newer versions take 16.
            (
                "refs",
                plain,
                "yuv420p",
                "hvc1",
                "keyint=30:ref=6:bframes=8:rc-lookahead=40",
            ),
            (
                "slices",
                plain,
                "yuv420p",
                "hvc1",
                "ctu=16:slices=4:keyint=15",
            ),
            ("wpp", plain, "yuv420p", "hvc1", "wpp=1:ctu=16:keyint=15"),
            (
                "weighted",
                fade,
                "yuv420p",
                "hvc1",
                "weightp=1:weightb=1:bframes=3:keyint=30",
            ),
            ("main10", plain, "yuv420p10le", "hvc1", "keyint=15"),
            ("yuv444", plain, "yuv444p", "hvc1", "keyint=15"),
            ("yuv422", plain, "yuv422p10le", "hvc1", "keyint=15"),
            (
                "inband",
                plain,
                "yuv420p",
                "hev1",
                "keyint=15:repeat-headers=1",
            ),
        ]
        .into_iter()
        .map(|(name, filters, format, tag, params)| {
            (name, encode(name, filters, format, tag, params))
        })
        .collect()
    }

    /// What encryption does to real streams: the subsample map accounts for every byte, the clear
    /// bytes (length prefixes, parameter sets, SEI, and every slice header) come out untouched,
    /// and the slice data is changed.
    #[test]
    fn encryption_changes_only_slice_data() {
        use super::super::cipher::{Cipher, Pattern};
        use super::super::segment::hevc_subsamples;

        if !encoder_available() {
            eprintln!("skipping: ffmpeg with libx265 is unavailable");
            return;
        }
        let cipher = Cipher::new(&[0x11; 16]);
        let mut protected_total = 0usize;
        for (name, path) in variants() {
            let (parameters, samples) = samples(&path);
            for (index, sample) in samples.iter().enumerate() {
                let map = hevc_subsamples(&parameters, sample).unwrap();
                let covered: usize = map
                    .iter()
                    .map(|&(clear, protected)| usize::from(clear) + protected as usize)
                    .sum();
                assert_eq!(covered, sample.len(), "{name} sample {index}");

                let mut encrypted = sample.clone();
                let mut at = 0;
                for &(clear, protected) in &map {
                    at += usize::from(clear);
                    let end = at + protected as usize;
                    cipher.encrypt_range(&[0x22; 16], &mut encrypted[at..end], Pattern::VIDEO);
                    at = end;
                    protected_total += protected as usize;
                }
                let mut at = 0;
                for &(clear, protected) in &map {
                    let end = at + usize::from(clear);
                    assert_eq!(encrypted[at..end], sample[at..end], "{name} sample {index}");
                    at = end + protected as usize;
                }
                assert_ne!(&encrypted, sample, "{name} sample {index} is encrypted");
            }
        }
        assert!(protected_total > 100_000, "{protected_total}");
    }

    #[test]
    fn what_cannot_be_encrypted_fails_closed() {
        use super::super::segment::hevc_subsamples;

        let mut parameters = HevcParameters::empty(4);
        let sample = |header: [u8; 2], body: &[u8]| {
            let mut bytes = u32::try_from(body.len() + 2)
                .unwrap()
                .to_be_bytes()
                .to_vec();
            bytes.extend_from_slice(&header);
            bytes.extend_from_slice(body);
            bytes
        };
        // A slice that names a parameter set the track never declared.
        let unknown = sample([1 << 1, 1], &[0x80, 0, 0, 0]);
        assert!(hevc_subsamples(&parameters, &unknown).is_err());
        // Reserved VCL types and layers above the base layer are refused, not guessed at.
        for header in [
            [22 << 1, 1],
            [10 << 1, 1],
            [31 << 1, 1],
            [1 << 1, 1 | (1 << 3)],
        ] {
            let error = hevc_subsamples(&parameters, &sample(header, &[0x80; 8])).unwrap_err();
            assert!(
                matches!(error, Error::Unsupported(_)),
                "{header:?}: {error}"
            );
        }
        // Parameter sets, SEI, and the like stay clear.
        let vps = sample([32 << 1, 1], &[1, 2, 3]);
        assert_eq!(hevc_subsamples(&parameters, &vps).unwrap(), [(9, 0)]);
        // Truncated input is an error, never a panic.
        for cut in 0..unknown.len() {
            let _ = hevc_subsamples(&parameters, &unknown[..cut]);
            let _ = parameters.update(&unknown[..cut]);
        }
        assert!(parameters.update(&[33 << 1, 1, 0xff]).is_err());
        assert!(parameters.clear_bytes(&[]).is_err());
    }

    #[test]
    fn every_slice_header_ends_where_ffmpeg_says_it_does() {
        if !encoder_available() {
            eprintln!("skipping: ffmpeg with libx265 is unavailable");
            return;
        }
        let mut slices_checked = 0;
        for (name, path) in variants() {
            let (parameters, samples) = samples(&path);
            let mut current = parameters.clone();
            let mut ours = Vec::new();
            for sample in &samples {
                for nal in nal_units(sample, parameters.nal_length_size()) {
                    let nal_type = (nal[0] >> 1) & 0x3f;
                    if matches!(nal_type, 32..=34) {
                        current.update(nal).unwrap();
                    }
                    if matches!(nal_type, 0..=9 | 16..=21) {
                        let rbsp = Rbsp::new(nal, LARGE_SLICE_HEADER_BYTES);
                        ours.push(current.header_bits(&rbsp).unwrap());
                        assert!(current.clear_bytes(nal).unwrap() < nal.len(), "{name}");
                    }
                }
            }
            let theirs = ffmpeg_header_ends(&["-i", path.to_str().unwrap()]).unwrap();
            assert_eq!(ours.len(), theirs.len(), "{name}: slice count");
            for (index, (ours, theirs)) in ours.iter().zip(&theirs).enumerate() {
                assert_eq!(ours, theirs, "{name}: slice {index}");
            }
            slices_checked += ours.len();
        }
        assert!(slices_checked > 300, "{slices_checked}");
    }
}

/// Hand-built streams for the syntax x265 does not produce: reference sets predicted from other
/// sets, long-term pictures, list modification, tiles, dependent slice segments, scaling lists,
/// and the rest. They are checked the same way, against `FFmpeg`'s own reading of the same bytes,
/// so the stream only has to parse alike, not be a good video.
#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::too_many_lines,
    reason = "a bitstream writer for tests: its values are small and chosen by the tests, and its structs mirror syntax elements"
)]
mod synthetic {
    use super::tests::{ffmpeg_available, ffmpeg_header_ends, nal_units_annex_b};
    use super::*;

    struct Bits {
        bytes: Vec<u8>,
        used: usize,
    }

    impl Bits {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                used: 8,
            }
        }
        fn bit(&mut self, value: bool) {
            if self.used == 8 {
                self.bytes.push(0);
                self.used = 0;
            }
            if value {
                *self.bytes.last_mut().unwrap() |= 0x80 >> self.used;
            }
            self.used += 1;
        }
        fn u(&mut self, width: usize, value: u64) {
            for shift in (0..width).rev() {
                self.bit(value >> shift & 1 == 1);
            }
        }
        fn ue(&mut self, value: u32) {
            let code = u64::from(value) + 1;
            let width = 64 - code.leading_zeros() as usize;
            self.u(width - 1, 0);
            self.u(width, code);
        }
        fn se(&mut self, value: i32) {
            self.ue(if value > 0 {
                2 * value as u32 - 1
            } else {
                (-2 * value) as u32
            });
        }
        fn finish(mut self) -> Vec<u8> {
            self.bit(true);
            while self.used != 8 {
                self.bit(false);
            }
            self.bytes
        }
    }

    /// An Annex B NAL unit: a start code, the two-byte header, and the payload with emulation
    /// prevention bytes inserted.
    fn nal(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 1, kind << 1, 1];
        let mut zeros = 0;
        for &byte in payload {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    fn profile_tier_level(bits: &mut Bits) {
        bits.u(2, 0);
        bits.bit(false);
        bits.u(5, 1);
        bits.u(32, 0x6000_0000);
        bits.u(4, 0b1000);
        bits.u(43, 0);
        bits.bit(false);
        bits.u(8, 93);
    }

    fn vps() -> Vec<u8> {
        let mut bits = Bits::new();
        bits.u(4, 0);
        bits.u(2, 3);
        bits.u(6, 0);
        bits.u(3, 0);
        bits.bit(true);
        bits.u(16, 0xffff);
        profile_tier_level(&mut bits);
        bits.bit(true);
        bits.ue(4);
        bits.ue(0);
        bits.ue(0);
        bits.u(6, 0);
        bits.ue(0);
        bits.bit(false);
        bits.bit(false);
        nal(32, &bits.finish())
    }

    #[derive(Clone)]
    enum Rps {
        Explicit {
            before: Vec<(u32, bool)>,
            after: Vec<(u32, bool)>,
        },
        /// Predicted from an earlier set; one `(used, use_delta)` per picture of that set, plus one.
        Inter {
            delta_idx_minus1: u32,
            negative: bool,
            abs_minus1: u32,
            flags: Vec<(bool, bool)>,
        },
    }

    fn write_rps(bits: &mut Bits, index: usize, count: usize, rps: &Rps) {
        if index != 0 {
            bits.bit(matches!(rps, Rps::Inter { .. }));
        }
        match rps {
            Rps::Explicit { before, after } => {
                bits.ue(before.len() as u32);
                bits.ue(after.len() as u32);
                for &(delta, used) in before.iter().chain(after) {
                    bits.ue(delta);
                    bits.bit(used);
                }
            }
            Rps::Inter {
                delta_idx_minus1,
                negative,
                abs_minus1,
                flags,
            } => {
                if index == count {
                    bits.ue(*delta_idx_minus1);
                }
                bits.bit(*negative);
                bits.ue(*abs_minus1);
                for &(used, use_delta) in flags {
                    bits.bit(used);
                    if !used {
                        bits.bit(use_delta);
                    }
                }
            }
        }
    }

    #[derive(Clone)]
    struct SpsSpec {
        width: u32,
        height: u32,
        chroma_format: u32,
        separate_colour_plane: bool,
        log2_ctb: u32,
        poc_bits: usize,
        scaling_lists: bool,
        pcm: bool,
        sao: bool,
        sets: Vec<Rps>,
        long_term: Option<Vec<(u32, bool)>>,
        temporal_mvp: bool,
    }

    impl Default for SpsSpec {
        fn default() -> Self {
            Self {
                width: 320,
                height: 192,
                chroma_format: 1,
                separate_colour_plane: false,
                log2_ctb: 4,
                poc_bits: 8,
                scaling_lists: false,
                pcm: false,
                sao: true,
                sets: Vec::new(),
                long_term: None,
                temporal_mvp: true,
            }
        }
    }

    fn scaling_list_data(bits: &mut Bits) {
        for size_id in 0..4u32 {
            for matrix in (0..6).step_by(if size_id == 3 { 3 } else { 1 }) {
                if size_id == 0 && matrix == 0 {
                    // One list written out, so the coefficient path is read too.
                    bits.bit(true);
                    for _ in 0..16 {
                        bits.se(1);
                    }
                } else if size_id == 2 && matrix == 1 {
                    bits.bit(true);
                    bits.se(-3);
                    for _ in 0..64 {
                        bits.se(-1);
                    }
                } else {
                    bits.bit(false);
                    bits.ue(0);
                }
            }
        }
    }

    fn sps(spec: &SpsSpec) -> Vec<u8> {
        let mut bits = Bits::new();
        bits.u(4, 0);
        bits.u(3, 0);
        bits.bit(true);
        profile_tier_level(&mut bits);
        bits.ue(0);
        bits.ue(spec.chroma_format);
        if spec.chroma_format == 3 {
            bits.bit(spec.separate_colour_plane);
        }
        bits.ue(spec.width);
        bits.ue(spec.height);
        bits.bit(false);
        bits.ue(0);
        bits.ue(0);
        bits.ue(spec.poc_bits as u32 - 4);
        bits.bit(true);
        bits.ue(4);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0); // min cb 8
        bits.ue(spec.log2_ctb - 3);
        bits.ue(0);
        bits.ue(2);
        bits.ue(1);
        bits.ue(1);
        bits.bit(spec.scaling_lists);
        if spec.scaling_lists {
            bits.bit(true);
            scaling_list_data(&mut bits);
        }
        bits.bit(true);
        bits.bit(spec.sao);
        bits.bit(spec.pcm);
        if spec.pcm {
            bits.u(4, 7);
            bits.u(4, 7);
            bits.ue(0);
            bits.ue(1);
            bits.bit(false);
        }
        bits.ue(spec.sets.len() as u32);
        for (index, set) in spec.sets.iter().enumerate() {
            write_rps(&mut bits, index, spec.sets.len(), set);
        }
        bits.bit(spec.long_term.is_some());
        if let Some(pictures) = &spec.long_term {
            bits.ue(pictures.len() as u32);
            for &(lsb, used) in pictures {
                bits.u(spec.poc_bits, u64::from(lsb));
                bits.bit(used);
            }
        }
        bits.bit(spec.temporal_mvp);
        bits.bit(false); // strong_intra_smoothing
        bits.bit(false); // vui
        bits.bit(false); // extensions
        nal(33, &bits.finish())
    }

    #[derive(Clone, Default)]
    struct PpsSpec {
        dependent_slices: bool,
        output_flag: bool,
        extra_bits: usize,
        cabac_init: bool,
        l0_default_minus1: u32,
        l1_default_minus1: u32,
        weighted_pred: bool,
        weighted_bipred: bool,
        slice_chroma_qp_offsets: bool,
        /// Columns and rows minus one, and whether spacing is uniform.
        tiles: Option<(u32, u32, bool)>,
        wavefronts: bool,
        deblocking_control: Option<(bool, bool)>,
        loop_filter_across_slices: bool,
        scaling_lists: bool,
        lists_modification: bool,
        header_extension: bool,
        chroma_qp_offset_list: bool,
        transform_skip: bool,
    }

    fn pps(spec: &PpsSpec) -> Vec<u8> {
        let mut bits = Bits::new();
        bits.ue(0);
        bits.ue(0);
        bits.bit(spec.dependent_slices);
        bits.bit(spec.output_flag);
        bits.u(3, spec.extra_bits as u64);
        bits.bit(false);
        bits.bit(spec.cabac_init);
        bits.ue(spec.l0_default_minus1);
        bits.ue(spec.l1_default_minus1);
        bits.se(0);
        bits.bit(false);
        bits.bit(spec.transform_skip);
        bits.bit(true);
        bits.ue(1);
        bits.se(0);
        bits.se(0);
        bits.bit(spec.slice_chroma_qp_offsets);
        bits.bit(spec.weighted_pred);
        bits.bit(spec.weighted_bipred);
        bits.bit(false);
        bits.bit(spec.tiles.is_some());
        bits.bit(spec.wavefronts);
        if let Some((columns, rows, uniform)) = spec.tiles {
            bits.ue(columns);
            bits.ue(rows);
            bits.bit(uniform);
            if !uniform {
                for _ in 0..columns + rows {
                    bits.ue(2);
                }
            }
            bits.bit(true);
        }
        bits.bit(spec.loop_filter_across_slices);
        bits.bit(spec.deblocking_control.is_some());
        if let Some((override_enabled, disabled)) = spec.deblocking_control {
            bits.bit(override_enabled);
            bits.bit(disabled);
            if !disabled {
                bits.se(1);
                bits.se(-1);
            }
        }
        bits.bit(spec.scaling_lists);
        if spec.scaling_lists {
            scaling_list_data(&mut bits);
        }
        bits.bit(spec.lists_modification);
        bits.ue(0);
        bits.bit(spec.header_extension);
        let extended = spec.chroma_qp_offset_list;
        bits.bit(extended);
        if extended {
            bits.bit(true);
            bits.u(3, 0);
            bits.u(4, 0);
            if spec.transform_skip {
                bits.ue(0);
            }
            bits.bit(false);
            bits.bit(true); // chroma_qp_offset_list_enabled_flag
            bits.ue(0);
            bits.ue(0);
            bits.se(1);
            bits.se(1);
            bits.ue(0);
            bits.ue(0);
        }
        nal(34, &bits.finish())
    }

    /// One slice segment header and a few bytes of stand-in slice data.
    #[derive(Clone)]
    struct Slice {
        kind: u8,
        first: bool,
        dependent: bool,
        address: u64,
        slice_type: u32,
        from_sps: Option<usize>,
        inline: Option<Rps>,
        /// Long-term pictures: how many come from the SPS (with their indexes), then explicit ones
        /// `(lsb, used, msb cycle)`.
        long_term_sps: Vec<usize>,
        long_term: Vec<(u32, bool, Option<u32>)>,
        temporal_mvp: bool,
        sao: (bool, bool),
        override_refs: Option<(u32, u32)>,
        list_modification: (bool, bool),
        pictures_total: usize,
        collocated_from_l0: bool,
        collocated_ref_idx: u32,
        deblocking_override: Option<(bool, bool)>,
        loop_filter_flag: bool,
        entry_points: u32,
        extension_bytes: u32,
    }

    impl Default for Slice {
        fn default() -> Self {
            Self {
                kind: 19,
                first: true,
                dependent: false,
                address: 0,
                slice_type: 2,
                from_sps: None,
                inline: None,
                long_term_sps: Vec::new(),
                long_term: Vec::new(),
                temporal_mvp: false,
                sao: (true, true),
                override_refs: None,
                list_modification: (false, false),
                pictures_total: 0,
                collocated_from_l0: true,
                collocated_ref_idx: 0,
                deblocking_override: None,
                loop_filter_flag: true,
                entry_points: 0,
                extension_bytes: 0,
            }
        }
    }

    fn ceil_log2(value: usize) -> usize {
        if value <= 1 {
            0
        } else {
            (usize::BITS - (value - 1).leading_zeros()) as usize
        }
    }

    fn slice(spec: &Slice, s: &SpsSpec, p: &PpsSpec) -> Vec<u8> {
        let mut bits = Bits::new();
        let idr = matches!(spec.kind, 19 | 20);
        bits.bit(spec.first);
        if (16..=23).contains(&spec.kind) {
            bits.bit(false);
        }
        bits.ue(0);
        if !spec.first {
            if p.dependent_slices {
                bits.bit(spec.dependent);
            }
            let ctb = 1u32 << s.log2_ctb;
            let size = u64::from(s.width.div_ceil(ctb) * s.height.div_ceil(ctb));
            bits.u(ceil_log2(size as usize), spec.address);
        }
        if !spec.dependent {
            for _ in 0..p.extra_bits {
                bits.bit(false);
            }
            bits.ue(spec.slice_type);
            if p.output_flag {
                bits.bit(true);
            }
            if s.separate_colour_plane {
                bits.u(2, 0);
            }
            let chroma = if s.separate_colour_plane {
                0
            } else {
                s.chroma_format
            };
            if !idr {
                bits.u(s.poc_bits, 3);
                bits.bit(spec.from_sps.is_some());
                if let Some(index) = spec.from_sps {
                    if s.sets.len() > 1 {
                        bits.u(ceil_log2(s.sets.len()), index as u64);
                    }
                } else {
                    write_rps(
                        &mut bits,
                        s.sets.len(),
                        s.sets.len(),
                        spec.inline.as_ref().unwrap(),
                    );
                }
                if let Some(in_sps) = &s.long_term {
                    if !in_sps.is_empty() {
                        bits.ue(spec.long_term_sps.len() as u32);
                    }
                    bits.ue(spec.long_term.len() as u32);
                    for &index in &spec.long_term_sps {
                        if in_sps.len() > 1 {
                            bits.u(ceil_log2(in_sps.len()), index as u64);
                        }
                        bits.bit(false);
                    }
                    for &(lsb, used, msb) in &spec.long_term {
                        bits.u(s.poc_bits, u64::from(lsb));
                        bits.bit(used);
                        bits.bit(msb.is_some());
                        if let Some(cycle) = msb {
                            bits.ue(cycle);
                        }
                    }
                }
                if s.temporal_mvp {
                    bits.bit(spec.temporal_mvp);
                }
            }
            if s.sao {
                bits.bit(spec.sao.0);
                if chroma != 0 {
                    bits.bit(spec.sao.1);
                }
            }
            let (b, pslice) = (spec.slice_type == 0, spec.slice_type == 1);
            if b || pslice {
                let (mut l0, mut l1) = (p.l0_default_minus1 + 1, p.l1_default_minus1 + 1);
                bits.bit(spec.override_refs.is_some());
                if let Some((first, second)) = spec.override_refs {
                    bits.ue(first);
                    l0 = first + 1;
                    if b {
                        bits.ue(second);
                        l1 = second + 1;
                    }
                }
                if p.lists_modification && spec.pictures_total > 1 {
                    let width = ceil_log2(spec.pictures_total);
                    bits.bit(spec.list_modification.0);
                    if spec.list_modification.0 {
                        for _ in 0..l0 {
                            bits.u(width, 0);
                        }
                    }
                    if b {
                        bits.bit(spec.list_modification.1);
                        if spec.list_modification.1 {
                            for _ in 0..l1 {
                                bits.u(width, 0);
                            }
                        }
                    }
                }
                if b {
                    bits.bit(false);
                }
                if p.cabac_init {
                    bits.bit(true);
                }
                if spec.temporal_mvp {
                    let from_l0 = if b {
                        bits.bit(spec.collocated_from_l0);
                        spec.collocated_from_l0
                    } else {
                        true
                    };
                    if (from_l0 && l0 > 1) || (!from_l0 && l1 > 1) {
                        bits.ue(spec.collocated_ref_idx);
                    }
                }
                if (p.weighted_pred && pslice) || (p.weighted_bipred && b) {
                    bits.ue(5);
                    if chroma != 0 {
                        bits.se(-1);
                    }
                    for count in [l0, if b { l1 } else { 0 }] {
                        for index in 0..count {
                            bits.bit(index % 2 == 0);
                        }
                        if chroma != 0 {
                            for index in 0..count {
                                bits.bit(index % 3 == 0);
                            }
                        }
                        for index in 0..count {
                            if index % 2 == 0 {
                                bits.se(2);
                                bits.se(-2);
                            }
                            if chroma != 0 && index % 3 == 0 {
                                for _ in 0..2 {
                                    bits.se(1);
                                    bits.se(-1);
                                }
                            }
                        }
                    }
                }
                bits.ue(2);
            }
            bits.se(-4);
            if p.slice_chroma_qp_offsets {
                bits.se(1);
                bits.se(-1);
            }
            if p.chroma_qp_offset_list {
                bits.bit(true);
            }
            let mut deblocking_disabled = p.deblocking_control.is_some_and(|(_, off)| off);
            if p.deblocking_control.is_some_and(|(allowed, _)| allowed) {
                bits.bit(spec.deblocking_override.is_some());
                if let Some((disabled, _)) = spec.deblocking_override {
                    bits.bit(disabled);
                    deblocking_disabled = disabled;
                    if !disabled {
                        bits.se(2);
                        bits.se(-2);
                    }
                }
            }
            if p.loop_filter_across_slices
                && ((s.sao && (spec.sao.0 || spec.sao.1)) || !deblocking_disabled)
            {
                bits.bit(spec.loop_filter_flag);
            }
        }
        if p.tiles.is_some() || p.wavefronts {
            bits.ue(spec.entry_points);
            if spec.entry_points > 0 {
                bits.ue(11);
                for _ in 0..spec.entry_points {
                    bits.u(12, 100);
                }
            }
        }
        if p.header_extension {
            bits.ue(spec.extension_bytes);
            for _ in 0..spec.extension_bytes {
                bits.u(8, 0xa5);
            }
        }
        let mut payload = bits.finish();
        payload.extend_from_slice(&[0x80, 0x44, 0x00, 0x00, 0x01, 0xfe]);
        nal(spec.kind, &payload)
    }

    /// Writes the stream, reads it with `FFmpeg` and with this parser, and compares every header.
    fn check(name: &str, s: &SpsSpec, p: &PpsSpec, slices: &[Slice]) {
        let mut stream = vps();
        stream.extend(sps(s));
        stream.extend(pps(p));
        // FFmpeg's copy mode drops packets until the first keyframe, so the stream starts with one.
        let mut slices = slices.to_vec();
        slices.insert(0, Slice::default());
        for spec in &slices {
            stream.extend(slice(spec, s, p));
        }
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/hevc-tests/synthetic-{name}.hevc"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &stream).unwrap();

        let units = nal_units_annex_b(&stream);
        let mut parameters = HevcParameters::empty(4);
        let mut ours = Vec::new();
        for unit in &units {
            let kind = (unit[0] >> 1) & 0x3f;
            if matches!(kind, 33 | 34) {
                parameters.update(unit).unwrap();
            } else if matches!(kind, 0..=9 | 16..=21) {
                let rbsp = Rbsp::new(unit, LARGE_SLICE_HEADER_BYTES);
                ours.push(
                    parameters
                        .header_bits(&rbsp)
                        .unwrap_or_else(|error| panic!("{name}: slice {}: {error}", ours.len())),
                );
            }
        }
        let theirs = ffmpeg_header_ends(&["-f", "hevc", "-i", path.to_str().unwrap()]).unwrap();
        assert_eq!(
            theirs.len(),
            slices.len(),
            "{name}: FFmpeg read every slice"
        );
        assert_eq!(ours, theirs, "{name}");
    }

    fn explicit(before: &[(u32, bool)], after: &[(u32, bool)]) -> Rps {
        Rps::Explicit {
            before: before.to_vec(),
            after: after.to_vec(),
        }
    }

    #[test]
    fn reference_sets_predicted_from_other_sets() {
        if !ffmpeg_available() {
            return;
        }
        // Set 0 keeps three pictures (two before, one after); set 1 is predicted from it, set 2
        // from set 1, so the pictures each carries come from the derivation, not from the bits.
        let sps = SpsSpec {
            sets: vec![
                explicit(&[(0, true), (2, false)], &[(0, true)]),
                Rps::Inter {
                    delta_idx_minus1: 0,
                    negative: true,
                    abs_minus1: 0,
                    flags: vec![(true, false), (false, true), (false, false), (true, false)],
                },
                Rps::Inter {
                    delta_idx_minus1: 0,
                    negative: false,
                    abs_minus1: 1,
                    flags: vec![(false, true), (true, false), (true, false), (false, false)],
                },
            ],
            ..SpsSpec::default()
        };
        let pps = PpsSpec {
            lists_modification: true,
            l0_default_minus1: 2,
            ..PpsSpec::default()
        };
        let mut slices = Vec::new();
        // The pictures each set uses for prediction, worked out by hand from H.265 7-61/7-62.
        for (index, used) in [(0, 2), (1, 2), (2, 1)] {
            slices.push(Slice {
                kind: 1,
                slice_type: 1,
                from_sps: Some(index),
                pictures_total: used,
                list_modification: (index % 2 == 0, false),
                ..Slice::default()
            });
        }
        // Inline sets: one explicit, one predicted from the last SPS set, one from the one before.
        slices.push(Slice {
            kind: 1,
            slice_type: 0,
            inline: Some(explicit(&[(0, true)], &[(1, true)])),
            pictures_total: 2,
            list_modification: (true, true),
            override_refs: Some((1, 0)),
            ..Slice::default()
        });
        for (delta, flags) in [
            (0, vec![(true, false), (false, true), (true, false)]),
            (
                1,
                vec![(false, true), (true, false), (true, false), (false, true)],
            ),
        ] {
            slices.push(Slice {
                kind: 1,
                slice_type: 1,
                inline: Some(Rps::Inter {
                    delta_idx_minus1: delta,
                    negative: delta == 0,
                    abs_minus1: 0,
                    flags,
                }),
                pictures_total: 2,
                ..Slice::default()
            });
        }
        check("inter-rps", &sps, &pps, &slices);
    }

    #[test]
    fn long_term_pictures_and_collocated_references() {
        if !ffmpeg_available() {
            return;
        }
        let sps = SpsSpec {
            sets: vec![explicit(&[(0, true)], &[])],
            long_term: Some(vec![(3, true), (9, false), (20, true)]),
            ..SpsSpec::default()
        };
        let pps = PpsSpec {
            lists_modification: true,
            cabac_init: true,
            weighted_pred: true,
            weighted_bipred: true,
            l0_default_minus1: 1,
            l1_default_minus1: 1,
            ..PpsSpec::default()
        };
        let slices = vec![
            Slice {
                kind: 1,
                slice_type: 0,
                from_sps: Some(0),
                long_term_sps: vec![0, 2],
                long_term: vec![(7, true, Some(2)), (11, false, None)],
                temporal_mvp: true,
                collocated_from_l0: false,
                collocated_ref_idx: 1,
                pictures_total: 4,
                list_modification: (true, true),
                override_refs: Some((2, 1)),
                ..Slice::default()
            },
            Slice {
                kind: 1,
                slice_type: 1,
                from_sps: Some(0),
                long_term_sps: vec![0],
                temporal_mvp: true,
                pictures_total: 2,
                ..Slice::default()
            },
            Slice {
                kind: 1,
                slice_type: 2,
                from_sps: Some(0),
                ..Slice::default()
            },
        ];
        check("long-term", &sps, &pps, &slices);
    }

    #[test]
    fn tiles_dependent_slice_segments_and_header_extensions() {
        if !ffmpeg_available() {
            return;
        }
        let pps = PpsSpec {
            dependent_slices: true,
            tiles: Some((2, 1, false)),
            wavefronts: false,
            header_extension: true,
            extra_bits: 2,
            output_flag: true,
            ..PpsSpec::default()
        };
        let slices = vec![
            Slice {
                entry_points: 5,
                extension_bytes: 3,
                ..Slice::default()
            },
            Slice {
                first: false,
                dependent: true,
                address: 40,
                entry_points: 2,
                ..Slice::default()
            },
            Slice {
                first: false,
                dependent: false,
                address: 100,
                entry_points: 0,
                extension_bytes: 1,
                ..Slice::default()
            },
            Slice {
                first: false,
                dependent: true,
                address: 200,
                entry_points: 4,
                extension_bytes: 9,
                ..Slice::default()
            },
        ];
        check("tiles", &SpsSpec::default(), &pps, &slices);
    }

    #[test]
    fn scaling_lists_deblocking_and_chroma_offsets() {
        if !ffmpeg_available() {
            return;
        }
        for (name, sps, pps) in [
            (
                "scaling-deblocking",
                SpsSpec {
                    scaling_lists: true,
                    pcm: true,
                    ..SpsSpec::default()
                },
                PpsSpec {
                    scaling_lists: true,
                    deblocking_control: Some((true, false)),
                    loop_filter_across_slices: true,
                    slice_chroma_qp_offsets: true,
                    ..PpsSpec::default()
                },
            ),
            (
                "range-extension",
                SpsSpec {
                    chroma_format: 3,
                    ..SpsSpec::default()
                },
                PpsSpec {
                    chroma_qp_offset_list: true,
                    transform_skip: true,
                    deblocking_control: Some((true, true)),
                    loop_filter_across_slices: true,
                    ..PpsSpec::default()
                },
            ),
            (
                "separate-planes",
                SpsSpec {
                    chroma_format: 3,
                    separate_colour_plane: true,
                    sao: false,
                    ..SpsSpec::default()
                },
                PpsSpec::default(),
            ),
        ] {
            let slices = vec![
                Slice {
                    deblocking_override: Some((false, false)),
                    ..Slice::default()
                },
                Slice {
                    deblocking_override: Some((true, false)),
                    sao: (false, false),
                    ..Slice::default()
                },
                Slice {
                    deblocking_override: None,
                    sao: (true, false),
                    ..Slice::default()
                },
                Slice {
                    sao: (false, false),
                    loop_filter_flag: false,
                    ..Slice::default()
                },
            ];
            check(name, &sps, &pps, &slices);
        }
    }
}
