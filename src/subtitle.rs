//! Sidecar `WebVTT` files: validation and timeline correction.
//!
//! A subtitle file is fetched once when its asset loads and held in memory. Packaging can move an
//! asset onto a shifted timeline (an edit list or a fragmented start time), so a cue authored
//! against the source would appear early or late; [`prepare`] adds that offset to every cue timing
//! line and leaves everything else in the file as it was.

use std::fmt::Write;

use bytes::Bytes;

use crate::error::{Error, Result};

/// One subtitle file of an asset. Before [`prepare`] `data` is what was fetched; afterwards it is
/// what is served.
#[derive(Debug, Clone)]
pub(crate) struct Subtitle {
    pub(crate) language: String,
    pub(crate) label: String,
    pub(crate) default: bool,
    pub(crate) forced: bool,
    pub(crate) data: Bytes,
}

/// Validates `data` as `WebVTT` and moves every cue `offset_ms` later.
///
/// The file must be UTF-8 and begin with `WEBVTT` (after an optional byte order mark). With no
/// offset the original bytes are returned untouched, but a cue timing line that cannot be read is
/// refused either way, so a bad file is caught when the asset loads and not when a viewer's
/// player meets it.
pub(crate) fn prepare(language: &str, data: &[u8], offset_ms: u64) -> Result<Bytes> {
    let refuse = |reason: &str| Error::InvalidMedia(format!("subtitle `{language}`: {reason}"));
    let text = std::str::from_utf8(data).map_err(|_| refuse("is not UTF-8"))?;
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let header_ok = body
        .strip_prefix("WEBVTT")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']));
    if !header_ok {
        return Err(refuse("does not begin with WEBVTT"));
    }

    let mut shifted = String::with_capacity(body.len() + body.len() / 8);
    for line in body.split_inclusive(['\n', '\r']) {
        let content = line.trim_end_matches(['\n', '\r']);
        let terminator = &line[content.len()..];
        if content.contains("-->") {
            let timing = shift_timing_line(content, offset_ms)
                .ok_or_else(|| refuse("has a cue timing line that cannot be read"))?;
            shifted.push_str(&timing);
        } else {
            shifted.push_str(content);
        }
        shifted.push_str(terminator);
    }
    if offset_ms == 0 {
        Ok(Bytes::copy_from_slice(data))
    } else {
        Ok(Bytes::from(shifted))
    }
}

/// `start --> end settings`, with both times moved. `None` when it is not a well-formed timing.
fn shift_timing_line(line: &str, offset_ms: u64) -> Option<String> {
    let (start, rest) = line.split_once("-->")?;
    let start = start.trim();
    let rest = rest.trim_start();
    let (end, settings) = rest
        .split_once([' ', '\t'])
        .map_or((rest, ""), |(end, settings)| (end, settings));
    let start = parse_timestamp(start)?.checked_add(offset_ms)?;
    let end = parse_timestamp(end)?.checked_add(offset_ms)?;
    let mut out = String::new();
    write!(
        out,
        "{} --> {}",
        format_timestamp(start),
        format_timestamp(end)
    )
    .ok()?;
    if !settings.is_empty() {
        out.push(' ');
        out.push_str(settings.trim_start());
    }
    Some(out)
}

/// `[hh:]mm:ss.ttt` in milliseconds. Hours may have more than two digits.
fn parse_timestamp(text: &str) -> Option<u64> {
    let (clock, millis) = text.split_once('.')?;
    if millis.len() != 3 || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let fields = clock.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [minutes, seconds] => ("0", *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => return None,
    };
    let number = |field: &str, at_least: usize, at_most: usize| -> Option<u64> {
        (field.bytes().all(|byte| byte.is_ascii_digit())
            && (at_least..=at_most).contains(&field.len()))
        .then(|| field.parse().ok())?
    };
    let hours = if fields.len() == 3 {
        number(hours, 2, 10)?
    } else {
        0
    };
    let minutes = number(minutes, 2, 2).filter(|minutes| *minutes < 60)?;
    let seconds = number(seconds, 2, 2).filter(|seconds| *seconds < 60)?;
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(seconds * 1000)?
        .checked_add(millis.parse::<u64>().ok()?)
}

fn format_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let seconds = milliseconds / 1000 % 60;
    format!(
        "{hours:02}:{minutes:02}:{seconds:02}.{:03}",
        milliseconds % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shifted(text: &str, offset_ms: u64) -> String {
        String::from_utf8(
            prepare("en", text.as_bytes(), offset_ms)
                .expect("valid")
                .to_vec(),
        )
        .expect("UTF-8")
    }

    #[test]
    fn every_cue_moves_and_the_rest_of_the_file_does_not() {
        let file = "WEBVTT - title\n\nNOTE a comment\n\nintro\n00:01.000 --> 00:02.500 line:90% align:start\nHello\n\n01:00:00.000 --> 01:00:01.000\nHour cue\n";

        let out = shifted(file, 2500);

        assert_eq!(
            out,
            "WEBVTT - title\n\nNOTE a comment\n\nintro\n00:00:03.500 --> 00:00:05.000 line:90% align:start\nHello\n\n01:00:02.500 --> 01:00:03.500\nHour cue\n"
        );
    }

    #[test]
    fn the_shift_carries_across_seconds_minutes_and_hours() {
        let out = shifted("WEBVTT\n\n59:59.900 --> 59:59.999\nx\n", 200);

        assert!(out.contains("01:00:00.100 --> 01:00:00.199"), "{out}");
    }

    #[test]
    fn no_offset_returns_the_original_bytes() {
        let file = "\u{feff}WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHi\r\n";

        assert_eq!(
            prepare("en", file.as_bytes(), 0).expect("valid"),
            file.as_bytes()
        );
    }

    #[test]
    fn line_endings_survive_a_shift() {
        let out = shifted("WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHi\r\n", 1000);

        assert_eq!(out, "WEBVTT\r\n\r\n00:00:02.000 --> 00:00:03.000\r\nHi\r\n");
    }

    #[test]
    fn bad_files_are_refused_and_name_the_language() {
        for (data, needle) in [
            (&b"\xff\xfe"[..], "not UTF-8"),
            (b"", "WEBVTT"),
            (b"WEBVTTX\n", "WEBVTT"),
            (b"1\n00:01,000 --> 00:02,000\nsrt\n", "WEBVTT"),
            (b"WEBVTT\n\n00:01.000 --> nonsense\nx\n", "timing"),
            (b"WEBVTT\n\n00:61.000 --> 00:62.000\nx\n", "timing"),
            (b"WEBVTT\n\n00:01.00 --> 00:02.000\nx\n", "timing"),
            (
                b"WEBVTT\n\n99999999999999999999:00:00.000 --> 00:02.000\nx\n",
                "timing",
            ),
        ] {
            let error = prepare("fr", data, 0)
                .expect_err("should be refused")
                .to_string();
            assert!(error.contains("`fr`") && error.contains(needle), "{error}");
        }
    }
}
