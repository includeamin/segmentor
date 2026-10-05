//! Sidecar subtitle files: `WebVTT` as it is, `SubRip` (SRT) converted to it, and timeline
//! correction.
//!
//! A subtitle file is fetched once when its asset loads and held in memory. Packaging can move an
//! asset onto a shifted timeline (an edit list or a fragmented start time), so a cue authored
//! against the source would appear early or late; [`prepare`] adds that offset to every cue timing
//! line and leaves everything else in the file as it was. A `SubRip` file is converted to `WebVTT`
//! first, so everything downstream (playlists, manifests, versions, the shift) sees only `WebVTT`.

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

/// Validates `data` as `WebVTT` (or converts it from `SubRip`) and moves every cue `offset_ms`
/// later.
///
/// The file must be UTF-8 and either begin with `WEBVTT` (after an optional byte order mark) or be
/// `SubRip`: numbered or bare cues of `HH:MM:SS,mmm --> HH:MM:SS,mmm` and text. With no offset a
/// `WebVTT` file's original bytes are returned untouched, but a cue timing line that cannot be
/// read is refused either way, so a bad file is caught when the asset loads and not when a
/// viewer's player meets it.
pub(crate) fn prepare(language: &str, data: &[u8], offset_ms: u64) -> Result<Bytes> {
    let refuse = |reason: &str| Error::InvalidMedia(format!("subtitle `{language}`: {reason}"));
    let text = std::str::from_utf8(data)
        .map_err(|_| refuse("is not UTF-8 (convert the file to UTF-8)"))?;
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let header_ok = body
        .strip_prefix("WEBVTT")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\n', '\r']));
    let converted = if header_ok {
        None
    } else if looks_like_srt(body) {
        Some(srt_to_webvtt(body).map_err(&refuse)?)
    } else {
        return Err(refuse(
            "does not begin with WEBVTT and is not SubRip (SRT) either",
        ));
    };
    let body = converted.as_deref().unwrap_or(body);

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
    if offset_ms == 0 && converted.is_none() {
        Ok(Bytes::copy_from_slice(data))
    } else {
        Ok(Bytes::from(shifted))
    }
}

/// Whether `body` reads as `SubRip`: a cue timing line within the first few lines.
fn looks_like_srt(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .any(|line| line.contains("-->"))
}

/// One cue: its times in milliseconds, and its text, already `WebVTT`.
struct Cue {
    start: u64,
    end: u64,
    text: String,
}

/// Converts `SubRip` text to `WebVTT`.
///
/// - Cues are blocks separated by blank lines: an optional number, a timing line, then text. The
///   number is dropped, and so are the screen coordinates some editors append to the timing line.
/// - Times may use `,` or `.` before the milliseconds, and short hour, minute, and fraction fields.
/// - Text keeps `<b>`, `<i>`, and `<u>`; `<font>` tags and `{\an8}`-style override blocks are
///   removed; every other `<`, `>`, and bare `&` is escaped so that it stays text.
/// - A cue with no text, or that ends no later than it starts, is dropped, as it could never be
///   shown. Cues are put in start order, which players expect and editors do not guarantee.
///
/// A timing line that cannot be read refuses the file, as it does for `WebVTT`.
fn srt_to_webvtt(body: &str) -> std::result::Result<String, &'static str> {
    const UNREADABLE: &str = "has a cue timing line that cannot be read";
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = normalized.lines().peekable();
    let mut cues: Vec<Cue> = Vec::new();
    let mut found_timing = false;
    while let Some(line) = lines.next() {
        let mut line = line.trim();
        if line.is_empty() {
            continue;
        }
        // The cue number, when there is one: the next line is then the timing.
        if !line.contains("-->") && line.bytes().all(|byte| byte.is_ascii_digit()) {
            let Some(next) = lines.next() else { break };
            line = next.trim();
        }
        if !line.contains("-->") {
            return Err(UNREADABLE);
        }
        found_timing = true;
        let (start, end) = srt_timing(line).ok_or(UNREADABLE)?;
        let mut text = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim().is_empty() {
                break;
            }
            text.push(srt_text(next.trim_end()));
            lines.next();
        }
        let text = text.join("\n");
        if end > start && !text.trim().is_empty() {
            cues.push(Cue { start, end, text });
        }
    }
    if !found_timing {
        return Err(UNREADABLE);
    }
    cues.sort_by_key(|cue| cue.start);
    let mut out = String::with_capacity(body.len() + body.len() / 4 + 16);
    out.push_str("WEBVTT\n");
    for cue in &cues {
        write!(
            out,
            "\n{} --> {}\n{}\n",
            format_timestamp(cue.start),
            format_timestamp(cue.end),
            cue.text
        )
        .expect("writing to a String cannot fail");
    }
    Ok(out)
}

/// `start --> end`, with anything after the end time (screen coordinates) ignored.
fn srt_timing(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let end = rest.split_whitespace().next()?;
    Some((srt_timestamp(start.trim())?, srt_timestamp(end)?))
}

/// `H:MM:SS,mmm` in milliseconds. The separator before the fraction may be `,` or `.`, the fraction
/// has one to three digits (`,5` is half a second), and the other fields one or two.
fn srt_timestamp(text: &str) -> Option<u64> {
    let (clock, fraction) = text.split_once([',', '.'])?;
    if fraction.is_empty()
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut fields = clock.split(':');
    let (hours, minutes, seconds) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some() {
        return None;
    }
    let number = |field: &str, at_most: usize| -> Option<u64> {
        (field.bytes().all(|byte| byte.is_ascii_digit()) && (1..=at_most).contains(&field.len()))
            .then(|| field.parse().ok())?
    };
    let hours = number(hours, 10)?;
    let minutes = number(minutes, 2).filter(|minutes| *minutes < 60)?;
    let seconds = number(seconds, 2).filter(|seconds| *seconds < 60)?;
    let milliseconds = format!("{fraction:0<3}").parse::<u64>().ok()?;
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(seconds * 1000)?
        .checked_add(milliseconds)
}

/// One line of `SubRip` text as `WebVTT` cue text: see [`srt_to_webvtt`].
fn srt_text(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    let mut rest = line;
    while let Some(character) = rest.chars().next() {
        let tail = &rest[character.len_utf8()..];
        match character {
            '{' if tail.starts_with('\\') => {
                if let Some(end) = tail.find('}') {
                    rest = &tail[end + 1..];
                } else {
                    out.push('{');
                    rest = tail;
                }
            }
            '<' => {
                let end = tail.find('>').filter(|end| *end <= 64);
                let tag = end.map(|end| tail[..end].trim().to_ascii_lowercase());
                match (end, tag.as_deref()) {
                    (Some(end), Some("b" | "i" | "u" | "/b" | "/i" | "/u")) => {
                        out.push('<');
                        out.push_str(tag.as_deref().unwrap_or_default());
                        out.push('>');
                        rest = &tail[end + 1..];
                    }
                    (Some(end), Some(tag))
                        if tag == "font" || tag == "/font" || tag.starts_with("font ") =>
                    {
                        rest = &tail[end + 1..];
                    }
                    _ => {
                        out.push_str("&lt;");
                        rest = tail;
                    }
                }
            }
            '>' => {
                out.push_str("&gt;");
                rest = tail;
            }
            '&' => {
                let entity = tail
                    .find(';')
                    .filter(|end| *end <= 8)
                    .map(|end| &tail[..end]);
                let known = entity.is_some_and(|name| {
                    matches!(name, "amp" | "lt" | "gt" | "nbsp" | "lrm" | "rlm")
                        || name.strip_prefix('#').is_some_and(|digits| {
                            let (radix, digits) = digits
                                .strip_prefix(['x', 'X'])
                                .map_or((10, digits), |hex| (16, hex));
                            !digits.is_empty() && u32::from_str_radix(digits, radix).is_ok()
                        })
                });
                out.push_str(if known { "&" } else { "&amp;" });
                rest = tail;
            }
            other => {
                out.push(other);
                rest = tail;
            }
        }
    }
    out
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
            (b"neither\nformat\n", "SubRip"),
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

    fn converted(text: &str) -> String {
        String::from_utf8(prepare("en", text.as_bytes(), 0).expect("valid").to_vec()).unwrap()
    }

    #[test]
    fn a_numbered_srt_file_becomes_webvtt() {
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello\n\n2\n00:00:03,000 --> 00:00:04,000\nline one\nline two\n";

        assert_eq!(
            converted(srt),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\n\n00:00:03.000 --> 00:00:04.000\nline one\nline two\n"
        );
    }

    #[test]
    fn srt_in_the_shapes_editors_write_it() {
        // A byte order mark, CRLF endings, no cue numbers, `.` before the milliseconds, short
        // fields, a one-digit fraction (half a second, not five milliseconds), and trailing blank
        // lines.
        let srt = "\u{feff}0:0:1.5 --> 0:0:2,25\r\nfirst\r\n\r\n\r\n10:05:00,000 --> 10:05:01,000\r\nlast\r\n\r\n\r\n";

        assert_eq!(
            converted(srt),
            "WEBVTT\n\n00:00:01.500 --> 00:00:02.250\nfirst\n\n10:05:00.000 --> 10:05:01.000\nlast\n"
        );
        // Old Mac line endings.
        assert_eq!(
            converted("1\r00:00:01,000 --> 00:00:02,000\rHi\r"),
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n"
        );
    }

    #[test]
    fn screen_coordinates_after_the_timing_are_dropped() {
        let out = converted("1\n00:00:01,000 --> 00:00:02,000 X1:63 X2:223 Y1:43 Y2:58\nHi\n");

        assert_eq!(out, "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n");
    }

    #[test]
    fn text_keeps_basic_tags_and_stays_text() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n<B>bold</B> <i>it</i> <u>u</u> <font color=\"#ff0000\">red</font>\n2 < 3 > 1 & more &amp; &#169; &copy;\n{\\an8}top --> arrow\n";

        let out = converted(srt);

        assert!(
            out.contains("<b>bold</b> <i>it</i> <u>u</u> red\n"),
            "{out}"
        );
        assert!(
            out.contains("2 &lt; 3 &gt; 1 &amp; more &amp; &#169; &amp;copy;\n"),
            "{out}"
        );
        assert!(out.contains("top --&gt; arrow"), "{out}");
        assert!(!out.contains('{'), "{out}");
    }

    #[test]
    fn cues_are_ordered_and_the_ones_that_could_never_show_are_dropped() {
        let srt = "1\n00:00:05,000 --> 00:00:06,000\nlate\n\n2\n00:00:01,000 --> 00:00:02,000\nearly\n\n3\n00:00:03,000 --> 00:00:03,000\nzero length\n\n4\n00:00:04,000 --> 00:00:03,000\nbackwards\n\n5\n00:00:07,000 --> 00:00:08,000\n\n\n6\n00:00:01,000 --> 00:00:02,000\nsame start, later in the file\n";

        let out = converted(srt);

        assert_eq!(
            out,
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nearly\n\n00:00:01.000 --> 00:00:02.000\nsame start, later in the file\n\n00:00:05.000 --> 00:00:06.000\nlate\n"
        );
    }

    #[test]
    fn a_converted_file_is_moved_like_any_other() {
        let out = shifted("1\n00:00:01,000 --> 00:00:02,000\nHi\n", 2500);

        assert_eq!(out, "WEBVTT\n\n00:00:03.500 --> 00:00:04.500\nHi\n");
    }

    #[test]
    fn a_converted_file_is_valid_webvtt_that_converts_to_itself() {
        let once = converted("1\n00:00:01,000 --> 00:00:02,000\nA & B <i>x</i>\n");

        // Run through the WebVTT path again: unchanged.
        assert_eq!(converted(&once), once);
    }

    #[test]
    fn malformed_srt_is_refused() {
        for (data, needle) in [
            ("1\n00:00:01,000 --> nonsense\nx\n", "timing"),
            ("1\n00:00:61,000 --> 00:00:62,000\nx\n", "timing"),
            ("1\n00:00:01,0000 --> 00:00:02,000\nx\n", "timing"),
            ("1\n00:01,000 --> 00:02,000\nx\n", "timing"),
            ("1\n00:00:01 --> 00:00:02\nx\n", "timing"),
            (
                "1\nnot a timing line\n00:00:01,000 --> 00:00:02,000\nx\n",
                "timing",
            ),
        ] {
            let error = prepare("de", data.as_bytes(), 0)
                .expect_err("should be refused")
                .to_string();
            assert!(
                error.contains("`de`") && error.contains(needle),
                "{data:?}: {error}"
            );
        }
        // A non-UTF-8 file says what to do about it.
        let error = prepare("de", b"1\n00:00:01,000 --> 00:00:02,000\nCaf\xe9\n", 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("UTF-8"), "{error}");
    }

    /// An independent `WebVTT` parser, `FFmpeg`'s, reads the converted file and writes the same cues
    /// back out as `SubRip`.
    #[test]
    fn ffmpeg_reads_the_converted_file() {
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            eprintln!("skipping: ffmpeg is not installed");
            return;
        }
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nplain\n\n2\n01:02:03,004 --> 01:02:04,000\n<i>two</i> & lines\nsecond line\n\n3\n00:00:05,000 --> 00:00:06,000\n2 < 3\n";
        let directory =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/subtitle-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("converted-{}.vtt", std::process::id()));
        std::fs::write(&path, converted(srt)).unwrap();

        let output = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&path)
            .args(["-f", "srt", "-"])
            .output()
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        let out = String::from_utf8_lossy(&output.stdout);
        // Times and words only: how `FFmpeg` writes tags and entities back differs between its
        // versions, and the unit tests above pin those down without it.
        for expected in [
            "00:00:01,000 --> 00:00:02,500",
            "01:02:03,004 --> 01:02:04,000",
            "00:00:05,000 --> 00:00:06,000",
            "plain",
            "second line",
        ] {
            assert!(out.contains(expected), "{expected:?} in {out}");
        }
    }

    #[test]
    fn a_large_srt_converts_in_bounded_time() {
        let mut srt = String::new();
        for cue in 0..20_000u64 {
            let start = cue * 1500;
            writeln!(
                srt,
                "{}\n{} --> {}\nline <i>{cue}</i> & more\n",
                cue + 1,
                format_timestamp(start).replace('.', ","),
                format_timestamp(start + 1000).replace('.', ","),
            )
            .unwrap();
        }
        let started = std::time::Instant::now();

        let out = converted(&srt);

        assert_eq!(out.matches("-->").count(), 20_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
