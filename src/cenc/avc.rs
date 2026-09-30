//! Just enough H.264 (ITU-T H.264, 7.3) to find where each slice header ends, so `cbcs` can leave
//! it clear (ISO/IEC 23001-7, 10.4). Every read is bounded by the NAL unit; anything this does
//! not understand fails closed.

use std::collections::HashMap;

use super::bits::{BitReader, Rbsp};
use crate::error::{Error, Result};
use crate::mp4::boxes::{Reader, optional_child};
use crate::mp4::codec::visual_entry;

pub(super) const MAX_SLICE_HEADER_BYTES: usize = 4096;
const MAX_PARAMETER_SET_BYTES: usize = 64 * 1024;
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

#[derive(Debug, Clone)]
struct Sps {
    chroma_array_type: u32,
    separate_colour_plane: bool,
    log2_max_frame_num: usize,
    poc_type: u32,
    log2_max_poc_lsb: usize,
    delta_pic_order_always_zero: bool,
    frame_mbs_only: bool,
}

/// The PPS flags are the syntax elements themselves (H.264 7.3.2.2), not a state machine.
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the PPS syntax elements"
)]
#[derive(Debug, Clone)]
pub(super) struct Pps {
    sps_id: u32,
    entropy_coding: bool,
    bottom_field_pic_order_in_frame_present: bool,
    num_ref_idx_l0_default: u32,
    num_ref_idx_l1_default: u32,
    weighted_pred: bool,
    weighted_bipred_idc: u32,
    deblocking_filter_control_present: bool,
    redundant_pic_cnt_present: bool,
}

/// A track's parameter sets and NAL length size, from `avcC`, updated by in-band sets.
#[derive(Debug, Clone)]
pub(crate) struct AvcParameters {
    nal_length_size: usize,
    sps: HashMap<u32, Sps>,
    pub(super) pps: HashMap<u32, Pps>,
    #[cfg(test)]
    raw_sps: Vec<Vec<u8>>,
    #[cfg(test)]
    raw_pps: Vec<Vec<u8>>,
}

impl AvcParameters {
    /// Reads `avcC` from a visual sample entry's payload.
    pub(crate) fn from_sample_entry(entry: &[u8]) -> Result<Self> {
        let (_, _, children) = visual_entry(entry)?;
        let config = optional_child(children, *b"avcC")?
            .ok_or_else(|| Error::Unsupported("H.264 without avcC".to_owned()))?;
        let mut reader = Reader::new(config.payload);
        reader.skip(4)?;
        let mut parameters = Self {
            nal_length_size: usize::from(reader.u8()? & 3) + 1,
            sps: HashMap::new(),
            pps: HashMap::new(),
            #[cfg(test)]
            raw_sps: Vec::new(),
            #[cfg(test)]
            raw_pps: Vec::new(),
        };
        let sps_count = reader.u8()? & 0x1f;
        for _ in 0..sps_count {
            let length = usize::from(reader.u16()?);
            parameters.update(reader.take(length)?)?;
        }
        let pps_count = reader.u8()?;
        for _ in 0..pps_count {
            let length = usize::from(reader.u16()?);
            parameters.update(reader.take(length)?)?;
        }
        Ok(parameters)
    }

    pub(crate) const fn nal_length_size(&self) -> usize {
        self.nal_length_size
    }

    /// Takes in an SPS or PPS NAL unit (header byte included); other NAL units are ignored.
    pub(crate) fn update(&mut self, nal: &[u8]) -> Result<()> {
        match nal.first().map(|header| header & 0x1f) {
            Some(7) => {
                let (id, sps) = parse_sps(nal)?;
                self.sps.insert(id, sps);
                #[cfg(test)]
                self.raw_sps.push(nal.to_vec());
            }
            Some(8) => {
                let (id, pps) = parse_pps(nal)?;
                self.pps.insert(id, pps);
                #[cfg(test)]
                self.raw_pps.push(nal.to_vec());
            }
            _ => {}
        }
        Ok(())
    }

    /// How many bytes of this slice NAL unit (header byte included) must stay clear.
    pub(crate) fn clear_bytes(&self, nal: &[u8]) -> Result<usize> {
        let bits = self.header_bits(nal)?;
        Rbsp::new(nal, MAX_SLICE_HEADER_BYTES)
            .nal_bytes_covering(bits)
            .ok_or_else(|| Error::InvalidMedia("a slice header is truncated".to_owned()))
    }

    /// The slice header's length in RBSP bits, NAL header byte included (H.264 7.3.3).
    pub(super) fn header_bits(&self, nal: &[u8]) -> Result<usize> {
        let rbsp = Rbsp::new(nal, MAX_SLICE_HEADER_BYTES);
        let mut r = rbsp.reader();
        r.skip(1)?;
        let nal_ref_idc = r.bits(2)?;
        let nal_type = r.bits(5)?;
        if !matches!(nal_type, 1 | 5) {
            return Err(Error::Unsupported(format!(
                "H.264 NAL unit type {nal_type} cannot be encrypted"
            )));
        }
        r.ue()?; // first_mb_in_slice
        let raw_type = r.ue()?;
        if raw_type > 9 {
            return Err(Error::InvalidMedia(
                "an H.264 slice type is out of range".to_owned(),
            ));
        }
        let slice_type = raw_type % 5;
        let pps = self
            .pps
            .get(&r.ue()?)
            .ok_or_else(|| Error::InvalidMedia("a slice names an unknown PPS".to_owned()))?;
        let sps = self
            .sps
            .get(&pps.sps_id)
            .ok_or_else(|| Error::InvalidMedia("a PPS names an unknown SPS".to_owned()))?;
        let (p, b, i, sp, si) = (
            slice_type == 0,
            slice_type == 1,
            slice_type == 2,
            slice_type == 3,
            slice_type == 4,
        );
        if sps.separate_colour_plane {
            r.skip(2)?;
        }
        r.skip(sps.log2_max_frame_num)?;
        let mut field_pic = false;
        if !sps.frame_mbs_only {
            field_pic = r.bit()?;
            if field_pic {
                r.skip(1)?;
            }
        }
        if nal_type == 5 {
            r.ue()?; // idr_pic_id
        }
        if sps.poc_type == 0 {
            r.skip(sps.log2_max_poc_lsb)?;
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                r.se()?;
            }
        }
        if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
            r.se()?;
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                r.se()?;
            }
        }
        if pps.redundant_pic_cnt_present {
            r.ue()?;
        }
        if b {
            r.skip(1)?; // direct_spatial_mv_pred_flag
        }
        let (mut l0, mut l1) = (pps.num_ref_idx_l0_default, pps.num_ref_idx_l1_default);
        if (p || sp || b) && r.bit()? {
            l0 = ref_count(&mut r)?;
            if b {
                l1 = ref_count(&mut r)?;
            }
        }
        if !i && !si {
            list_modification(&mut r)?;
            if b {
                list_modification(&mut r)?;
            }
        }
        if (pps.weighted_pred && (p || sp)) || (pps.weighted_bipred_idc == 1 && b) {
            pred_weight_table(&mut r, sps.chroma_array_type, l0, if b { l1 } else { 0 })?;
        }
        if nal_ref_idc != 0 {
            dec_ref_pic_marking(&mut r, nal_type == 5)?;
        }
        if pps.entropy_coding && !i && !si {
            r.ue()?; // cabac_init_idc
        }
        r.se()?; // slice_qp_delta
        if sp || si {
            if sp {
                r.skip(1)?;
            }
            r.se()?;
        }
        if pps.deblocking_filter_control_present && r.ue()? != 1 {
            r.se()?;
            r.se()?;
        }
        Ok(r.position())
    }
}

fn ref_count(r: &mut BitReader<'_>) -> Result<u32> {
    let count = r.ue()?.checked_add(1).filter(|count| *count <= 32);
    count.ok_or_else(|| Error::InvalidMedia("too many H.264 reference indices".to_owned()))
}

fn list_modification(r: &mut BitReader<'_>) -> Result<()> {
    if !r.bit()? {
        return Ok(());
    }
    for _ in 0..=64 {
        match r.ue()? {
            0..=2 => {
                r.ue()?;
            }
            3 => return Ok(()),
            _ => break,
        }
    }
    Err(Error::InvalidMedia(
        "an H.264 reference list modification is malformed".to_owned(),
    ))
}

fn pred_weight_table(r: &mut BitReader<'_>, chroma: u32, l0: u32, l1: u32) -> Result<()> {
    r.ue()?;
    if chroma != 0 {
        r.ue()?;
    }
    for _ in 0..l0.saturating_add(l1) {
        if r.bit()? {
            r.se()?;
            r.se()?;
        }
        if chroma != 0 && r.bit()? {
            for _ in 0..4 {
                r.se()?;
            }
        }
    }
    Ok(())
}

fn dec_ref_pic_marking(r: &mut BitReader<'_>, idr: bool) -> Result<()> {
    if idr {
        return r.skip(2);
    }
    if !r.bit()? {
        return Ok(());
    }
    for _ in 0..=64 {
        let operation = r.ue()?;
        match operation {
            0 => return Ok(()),
            1 | 2 | 4 | 6 => {
                r.ue()?;
            }
            3 => {
                r.ue()?;
                r.ue()?;
            }
            5 => {}
            _ => break,
        }
    }
    Err(Error::InvalidMedia(
        "an H.264 reference marking is malformed".to_owned(),
    ))
}

fn parse_sps(nal: &[u8]) -> Result<(u32, Sps)> {
    let rbsp = Rbsp::new(nal, MAX_PARAMETER_SET_BYTES);
    let mut r = rbsp.reader();
    r.skip(8)?;
    let profile_idc = r.bits(8)?;
    r.skip(16)?;
    let id = r.ue()?;
    let (mut chroma_format_idc, mut separate) = (1, false);
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            return Err(Error::InvalidMedia(
                "an H.264 chroma format is out of range".to_owned(),
            ));
        }
        if chroma_format_idc == 3 {
            separate = r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
        if r.bit()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for index in 0..lists {
                if r.bit()? {
                    skip_scaling_list(&mut r, if index < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = small(r.ue()?, 12)? + 4;
    let poc_type = r.ue()?;
    let (mut log2_max_poc_lsb, mut delta_pic_order_always_zero) = (0, false);
    match poc_type {
        0 => log2_max_poc_lsb = small(r.ue()?, 12)? + 4,
        1 => {
            delta_pic_order_always_zero = r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..small(r.ue()?, 255)? {
                r.se()?;
            }
        }
        2 => {}
        _ => {
            return Err(Error::InvalidMedia(
                "an H.264 POC type is out of range".to_owned(),
            ));
        }
    }
    r.ue()?;
    r.skip(1)?;
    r.ue()?;
    r.ue()?;
    let frame_mbs_only = r.bit()?;
    Ok((
        id,
        Sps {
            chroma_array_type: if separate { 0 } else { chroma_format_idc },
            separate_colour_plane: separate,
            log2_max_frame_num,
            poc_type,
            log2_max_poc_lsb,
            delta_pic_order_always_zero,
            frame_mbs_only,
        },
    ))
}

fn parse_pps(nal: &[u8]) -> Result<(u32, Pps)> {
    let rbsp = Rbsp::new(nal, MAX_PARAMETER_SET_BYTES);
    let mut r = rbsp.reader();
    r.skip(8)?;
    let id = r.ue()?;
    let sps_id = r.ue()?;
    let entropy_coding = r.bit()?;
    let bottom_field_pic_order_in_frame_present = r.bit()?;
    if r.ue()? != 0 {
        return Err(Error::Unsupported(
            "H.264 with slice groups (FMO) cannot be encrypted".to_owned(),
        ));
    }
    let num_ref_idx_l0_default = ref_count(&mut r)?;
    let num_ref_idx_l1_default = ref_count(&mut r)?;
    let weighted_pred = r.bit()?;
    let weighted_bipred_idc = r.bits(2)?;
    r.se()?;
    r.se()?;
    r.se()?;
    let deblocking_filter_control_present = r.bit()?;
    r.skip(1)?;
    let redundant_pic_cnt_present = r.bit()?;
    Ok((
        id,
        Pps {
            sps_id,
            entropy_coding,
            bottom_field_pic_order_in_frame_present,
            num_ref_idx_l0_default,
            num_ref_idx_l1_default,
            weighted_pred,
            weighted_bipred_idc,
            deblocking_filter_control_present,
            redundant_pic_cnt_present,
        },
    ))
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Result<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

fn small(value: u32, max: u32) -> Result<usize> {
    if value > max {
        return Err(Error::InvalidMedia(
            "an H.264 parameter is out of range".to_owned(),
        ));
    }
    usize::try_from(value)
        .map_err(|_| Error::InvalidMedia("an H.264 parameter is out of range".to_owned()))
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::AvcParameters;

    /// The first IDR slice NAL unit of `name`'s first sample, with the track's parameters.
    pub(crate) fn first_idr(name: &str) -> (AvcParameters, Vec<u8>) {
        let (parameters, samples) = super::tests::samples(name);
        let length = parameters.nal_length_size();
        let sample = &samples[0];
        let mut at = 0;
        while at < sample.len() {
            let size = sample[at..at + length]
                .iter()
                .fold(0usize, |v, b| v << 8 | usize::from(*b));
            let nal = &sample[at + length..at + length + size];
            if nal[0] & 0x1f == 5 {
                return (parameters, nal.to_vec());
            }
            at += length + size;
        }
        panic!("{name} starts with an IDR slice");
    }
}

impl AvcParameters {
    /// Removes and returns one SPS and one PPS as NAL units, for in-band tests.
    #[cfg(test)]
    pub(crate) fn take_all_for_test(&mut self) -> (Vec<u8>, Vec<u8>) {
        let sps = std::mem::take(&mut self.raw_sps);
        let pps = std::mem::take(&mut self.raw_pps);
        self.sps.clear();
        self.pps.clear();
        (
            sps.into_iter().next().unwrap(),
            pps.into_iter().next().unwrap(),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::LimitsConfig;
    use crate::media::{MediaIndex, TrackKind};
    use crate::source::{ByteRange, LocalMediaSource, MediaSourceKind};
    use crate::testutil::fixture;

    /// The fixture's parameters and every video sample's bytes.
    pub(crate) fn samples(name: &str) -> (AvcParameters, Vec<Vec<u8>>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let source =
                MediaSourceKind::Local(Arc::new(LocalMediaSource::open(fixture(name)).unwrap()));
            let parsed = crate::mp4::parse(&source, &LimitsConfig::default())
                .await
                .unwrap();
            let index: &MediaIndex = &parsed.index;
            let video = index
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Video)
                .unwrap();
            let (_, entry) = crate::fmp4::sample_entry(&parsed.metadata, video.id).unwrap();
            let parameters = AvcParameters::from_sample_entry(&entry).unwrap();
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

    #[test]
    fn every_slice_header_ends_where_cabac_alignment_begins() {
        // The fixtures are CABAC: after the slice header come cabac_alignment_one_bit bits, all
        // ones, up to a byte boundary. A parse that stopped early or late would land on bits that
        // are not all ones (or on the wrong byte), so this checks every header's exact length.
        for name in ["h264-aac.mp4", "rendition-720p.mp4"] {
            let (parameters, samples) = samples(name);
            let mut slices = 0;
            for sample in &samples {
                for nal in nal_units(sample, parameters.nal_length_size()) {
                    if !matches!(nal[0] & 0x1f, 1 | 5) {
                        continue;
                    }
                    let bits = parameters.header_bits(nal).unwrap();
                    let rbsp = Rbsp::new(nal, MAX_SLICE_HEADER_BYTES);
                    let mut reader = rbsp.reader();
                    reader.skip(bits).unwrap();
                    while !reader.position().is_multiple_of(8) {
                        assert!(reader.bit().unwrap(), "{name}: alignment bits are ones");
                    }
                    assert_eq!(
                        parameters.clear_bytes(nal).unwrap(),
                        rbsp.nal_bytes_covering(bits).unwrap()
                    );
                    assert!(parameters.clear_bytes(nal).unwrap() < nal.len());
                    slices += 1;
                }
            }
            assert!(slices >= 90, "{name}: every frame has a slice ({slices})");
        }
    }

    #[test]
    fn an_in_band_pps_is_used_for_the_slices_after_it() {
        let (mut parameters, samples) = samples("h264-aac.mp4");
        let slice = nal_units(&samples[0], parameters.nal_length_size())
            .into_iter()
            .find(|nal| nal[0] & 0x1f == 5)
            .unwrap()
            .to_vec();
        let expected = parameters.clear_bytes(&slice).unwrap();
        // Move the only PPS to an in-band copy: without it the slice cannot be parsed, with it
        // the parse is the same.
        let pps = parameters.pps.drain().next().unwrap();
        assert!(parameters.clear_bytes(&slice).is_err(), "no PPS, no parse");
        parameters.pps.insert(pps.0, pps.1);
        assert_eq!(parameters.clear_bytes(&slice).unwrap(), expected);
    }

    #[test]
    fn unsupported_or_corrupt_slices_fail_closed() {
        let (parameters, _) = samples("h264-aac.mp4");

        assert!(parameters.clear_bytes(&[0x65]).is_err(), "truncated");
        assert!(
            parameters.clear_bytes(&[0x65, 0x88, 0xff, 0xff]).is_err(),
            "unknown PPS"
        );
    }
}
