//! TTML and DFXP (W3C Timed Text Markup Language) read as cues, for conversion to `WebVTT`.
//!
//! Only the part of TTML that carries words and times is read: `<p>` elements with their `begin`,
//! `end`, and `dur`, inside `<body>` and `<div>` containers that may offset them; line breaks; and
//! italic, bold, and underline, whether set on the element or through a referenced `<style>`.
//! Regions, colours, fonts, and positions are not carried over, as the roadmap keeps styling out of
//! scope. Everything is bounded and fails closed: an unsupported time base, a `DOCTYPE`, malformed
//! XML, or a time expression that cannot be read refuses the file.

use std::collections::HashMap;

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

use super::Cue;

const MAX_DEPTH: usize = 64;
/// How many `style` references are followed before giving up, so a cycle cannot loop.
const MAX_STYLE_DEPTH: usize = 8;
const MAX_TEXT_BYTES: usize = 64 * 1024;

type Refusal = &'static str;

/// Whether `body` has a TTML (or DFXP) root element: `tt`, with or without a namespace prefix,
/// after an optional XML declaration and comments.
pub(super) fn looks_like_ttml(body: &str) -> bool {
    let mut rest = body.strip_prefix('\u{feff}').unwrap_or(body).trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("<?") {
            let Some((_, tail)) = after.split_once("?>") else {
                return false;
            };
            rest = tail.trim_start();
        } else if let Some(after) = rest.strip_prefix("<!--") {
            let Some((_, tail)) = after.split_once("-->") else {
                return false;
            };
            rest = tail.trim_start();
        } else {
            break;
        }
    }
    let Some(element) = rest.strip_prefix('<') else {
        return false;
    };
    let name = element
        .split(|character: char| character.is_whitespace() || matches!(character, '>' | '/'))
        .next()
        .unwrap_or_default();
    name.rsplit(':').next() == Some("tt")
}

/// What `ttp:frameRate`, `ttp:frameRateMultiplier`, and `ttp:tickRate` make of `f` and `t` times.
#[derive(Clone, Copy)]
struct Timing {
    /// Frames per second, as a fraction.
    frames: (u64, u64),
    ticks_per_second: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            frames: (30, 1),
            ticks_per_second: 1,
        }
    }
}

/// The text style a cue can carry into `WebVTT`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    italic: bool,
    bold: bool,
    underline: bool,
}

impl Style {
    fn or(self, other: Self) -> Self {
        Self {
            italic: self.italic || other.italic,
            bold: self.bold || other.bold,
            underline: self.underline || other.underline,
        }
    }

    /// What is in `self` and not in `other`.
    fn without(self, other: Self) -> Self {
        Self {
            italic: self.italic && !other.italic,
            bold: self.bold && !other.bold,
            underline: self.underline && !other.underline,
        }
    }
}

/// A `<style>` element: its own properties and the styles it refers to.
struct StyleDefinition {
    own: Style,
    references: Vec<String>,
}

#[derive(PartialEq, Eq)]
enum Kind {
    Body,
    Div,
    Paragraph,
    Span,
    Other,
}

/// One open element.
struct Frame {
    kind: Kind,
    /// Where its children's times are counted from, and when it ends, in milliseconds.
    begin: u64,
    end: Option<u64>,
    /// The inherited style before this element, restored when it ends.
    restore: Style,
    /// The style tags this element wrote into the cue text, closed again when it ends.
    opened: Style,
}

/// The paragraph being read.
struct Paragraph {
    start: u64,
    end: Option<u64>,
    text: String,
    /// `xml:space="preserve"`: keep the text's spaces and line breaks.
    preserve: bool,
}

struct Converter {
    timing: Timing,
    styles: HashMap<String, StyleDefinition>,
    stack: Vec<Frame>,
    /// The style every element inherits here.
    effective: Style,
    /// The style tags open in the paragraph's text.
    written: Style,
    paragraph: Option<Paragraph>,
    cues: Vec<Cue>,
    /// Whether the root is in a TTML namespace; a file with no namespaces at all is read as TTML.
    namespaced: bool,
    seen_root: bool,
}

/// Reads every cue of a TTML document, in document order.
pub(super) fn ttml_cues(body: &str) -> Result<Vec<Cue>, Refusal> {
    const MALFORMED: Refusal = "is not well-formed XML";
    let mut reader = NsReader::from_str(body);
    let mut converter = Converter {
        timing: Timing::default(),
        styles: HashMap::new(),
        stack: Vec::new(),
        effective: Style::default(),
        written: Style::default(),
        paragraph: None,
        cues: Vec::new(),
        namespaced: false,
        seen_root: false,
    };
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|_| MALFORMED)?;
        let ttml = match namespace {
            ResolveResult::Bound(namespace) => Some(is_ttml_namespace(namespace.as_ref())),
            ResolveResult::Unbound => None,
            ResolveResult::Unknown(_) => Some(false),
        };
        match event {
            Event::Eof => break,
            Event::DocType(_) => return Err("has a DOCTYPE, which is not accepted"),
            Event::Start(start) => converter.start(&start, ttml)?,
            Event::Empty(start) => {
                converter.start(&start, ttml)?;
                converter.end()?;
            }
            Event::End(_) => converter.end()?,
            Event::Text(text) => converter.text(&text.xml10_content())?,
            Event::CData(text) => converter.text(&text.xml10_content())?,
            Event::GeneralRef(reference) => {
                let name = reference.xml10_content();
                let resolved = match reference.resolve_char_ref().map_err(|_| MALFORMED)? {
                    Some(character) => character.to_string(),
                    None => resolve_predefined_entity(&name)
                        .ok_or("uses an entity it does not define")?
                        .to_owned(),
                };
                converter.text(&resolved)?;
            }
            _ => {}
        }
    }
    if !converter.seen_root || !converter.stack.is_empty() {
        return Err(MALFORMED);
    }
    Ok(converter.cues)
}

fn is_ttml_namespace(namespace: &str) -> bool {
    namespace.starts_with("http://www.w3.org/ns/ttml")
        || namespace.starts_with("http://www.w3.org/2006/04/ttaf1")
        || namespace.starts_with("http://www.w3.org/2006/10/ttaf1")
}

/// The value of the attribute whose local name (without any prefix) is `local`.
fn attribute(element: &BytesStart<'_>, local: &str) -> Option<String> {
    element
        .attributes()
        .filter_map(Result::ok)
        .find_map(|attribute| {
            (attribute.key.local_name().as_ref() == local)
                .then(|| attribute.normalized_value(XmlVersion::Implicit1_0).ok())?
                .map(std::borrow::Cow::into_owned)
        })
}

impl Converter {
    fn start(&mut self, element: &BytesStart<'_>, ttml: Option<bool>) -> Result<(), Refusal> {
        if self.stack.len() >= MAX_DEPTH {
            return Err("is nested too deeply");
        }
        let local = element.local_name();
        let local = local.as_ref();
        if !self.seen_root {
            self.seen_root = true;
            self.namespaced = ttml == Some(true);
            if local != "tt" {
                return Err("does not have a tt root element");
            }
            self.read_parameters(element)?;
        }
        // Elements are TTML's when bound to its namespace; a document with no namespaces at all
        // is read as TTML too.
        let recognized = ttml == Some(true) || (!self.namespaced && ttml.is_none());
        let (parent_begin, parent_end) = self
            .stack
            .last()
            .map_or((0, None), |frame| (frame.begin, frame.end));
        let kind = match (recognized, local) {
            (true, "body") => Kind::Body,
            (true, "div") => Kind::Div,
            (true, "p") => Kind::Paragraph,
            (true, "span") if self.paragraph.is_some() => Kind::Span,
            (true, "style") => {
                self.define_style(element);
                Kind::Other
            }
            (true, "br") => {
                if let Some(paragraph) = &mut self.paragraph {
                    paragraph.text.push('\n');
                }
                Kind::Other
            }
            _ => Kind::Other,
        };
        let (begin, end) = self.times(element, parent_begin, parent_end)?;
        let restore = self.effective;
        let mut opened = Style::default();
        if matches!(kind, Kind::Body | Kind::Div | Kind::Paragraph | Kind::Span) {
            self.effective = self.effective.or(self.style_of(element));
        }
        if kind == Kind::Paragraph {
            if self.paragraph.is_some() {
                return Err("nests a paragraph in a paragraph");
            }
            self.paragraph = Some(Paragraph {
                start: begin,
                end,
                text: String::new(),
                preserve: attribute(element, "space").as_deref() == Some("preserve"),
            });
        }
        if matches!(kind, Kind::Paragraph | Kind::Span) {
            opened = self.effective.without(self.written);
            self.write_tags(opened, true);
            self.written = self.written.or(opened);
        }
        self.stack.push(Frame {
            kind,
            begin,
            end,
            restore,
            opened,
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), Refusal> {
        let frame = self.stack.pop().ok_or("is not well-formed XML")?;
        if matches!(frame.kind, Kind::Paragraph | Kind::Span) {
            self.write_tags(frame.opened, false);
            self.written = self.written.without(frame.opened);
        }
        self.effective = frame.restore;
        if frame.kind == Kind::Paragraph {
            let paragraph = self.paragraph.take().ok_or("is not well-formed XML")?;
            let end = paragraph.end.ok_or("has a cue with no end time")?;
            let text = tidy(&paragraph.text, paragraph.preserve);
            if end > paragraph.start && !text.trim().is_empty() {
                self.cues.push(Cue {
                    start: paragraph.start,
                    end,
                    text,
                });
            }
        }
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), Refusal> {
        let Some(paragraph) = &mut self.paragraph else {
            return Ok(());
        };
        for character in text.chars() {
            match character {
                '&' => paragraph.text.push_str("&amp;"),
                '<' => paragraph.text.push_str("&lt;"),
                '>' => paragraph.text.push_str("&gt;"),
                // Line breaks inside the text are white space, unless the author asked to keep them;
                // `<br/>` is what makes a line break.
                '\n' | '\r' | '\t' if !paragraph.preserve => paragraph.text.push(' '),
                other => paragraph.text.push(other),
            }
        }
        if paragraph.text.len() > MAX_TEXT_BYTES {
            return Err("has a cue with too much text");
        }
        Ok(())
    }

    /// The root's `ttp:` parameters: the frame and tick rates, and the time base.
    fn read_parameters(&mut self, root: &BytesStart<'_>) -> Result<(), Refusal> {
        if attribute(root, "timeBase").is_some_and(|base| base != "media") {
            return Err("uses a time base other than media, which is not supported");
        }
        let number =
            |name: &str| attribute(root, name).and_then(|text| text.trim().parse::<u64>().ok());
        let rate = number("frameRate");
        let (mut numerator, mut denominator) = (rate.unwrap_or(30), 1);
        if let Some(multiplier) = attribute(root, "frameRateMultiplier") {
            let mut parts = multiplier.split_whitespace().map(str::parse::<u64>);
            let (Some(Ok(top)), Some(Ok(bottom))) = (parts.next(), parts.next()) else {
                return Err("has a frame rate multiplier that cannot be read");
            };
            if top == 0 || bottom == 0 {
                return Err("has a frame rate multiplier that cannot be read");
            }
            numerator = numerator.saturating_mul(top);
            denominator = bottom;
        }
        if numerator == 0 {
            return Err("has a frame rate of zero");
        }
        let sub_frames = number("subFrameRate").unwrap_or(1);
        let ticks = number("tickRate")
            .or_else(|| rate.map(|rate| rate.saturating_mul(sub_frames)))
            .unwrap_or(1);
        if ticks == 0 {
            return Err("has a tick rate of zero");
        }
        self.timing = Timing {
            frames: (numerator, denominator),
            ticks_per_second: ticks,
        };
        Ok(())
    }

    /// An element's begin and end, in milliseconds on the document's clock: `begin`, `end`, and
    /// `dur` count from the start of the parent (a `par` time container); with no `end` or `dur`,
    /// an element lasts as long as its parent.
    fn times(
        &self,
        element: &BytesStart<'_>,
        parent_begin: u64,
        parent_end: Option<u64>,
    ) -> Result<(u64, Option<u64>), Refusal> {
        const UNREADABLE: Refusal = "has a time expression that cannot be read";
        let read = |name: &str| -> Result<Option<u64>, Refusal> {
            attribute(element, name)
                .map(|text| parse_time(&text, self.timing).ok_or(UNREADABLE))
                .transpose()
        };
        let begin = parent_begin
            .checked_add(read("begin")?.unwrap_or(0))
            .ok_or(UNREADABLE)?;
        let end = match (read("end")?, read("dur")?) {
            (Some(end), _) => Some(parent_begin.checked_add(end).ok_or(UNREADABLE)?),
            (None, Some(duration)) => Some(begin.checked_add(duration).ok_or(UNREADABLE)?),
            (None, None) => parent_end,
        };
        Ok((begin, end))
    }

    /// Records a `<style xml:id="...">`.
    fn define_style(&mut self, element: &BytesStart<'_>) {
        let Some(id) = attribute(element, "id") else {
            return;
        };
        let references = attribute(element, "style")
            .map(|text| text.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        self.styles.insert(
            id,
            StyleDefinition {
                own: own_style(element),
                references,
            },
        );
    }

    /// The style an element sets, through its own attributes and the styles it refers to.
    fn style_of(&self, element: &BytesStart<'_>) -> Style {
        let references = attribute(element, "style").unwrap_or_default();
        references
            .split_whitespace()
            .fold(own_style(element), |style, id| {
                style.or(self.referenced(id, MAX_STYLE_DEPTH))
            })
    }

    fn referenced(&self, id: &str, depth: usize) -> Style {
        let Some(definition) = self.styles.get(id).filter(|_| depth > 0) else {
            return Style::default();
        };
        definition
            .references
            .iter()
            .fold(definition.own, |style, id| {
                style.or(self.referenced(id, depth - 1))
            })
    }

    /// Writes the tags for `style` into the cue text: opening them, or closing them in reverse.
    fn write_tags(&mut self, style: Style, open: bool) {
        let Some(paragraph) = &mut self.paragraph else {
            return;
        };
        let mut tags = Vec::new();
        for (wanted, name) in [
            (style.italic, "i"),
            (style.bold, "b"),
            (style.underline, "u"),
        ] {
            if wanted {
                tags.push(name);
            }
        }
        if !open {
            tags.reverse();
        }
        for name in tags {
            paragraph.text.push_str(if open { "<" } else { "</" });
            paragraph.text.push_str(name);
            paragraph.text.push('>');
        }
    }
}

/// The style set by an element's own `tts:fontStyle`, `tts:fontWeight`, and `tts:textDecoration`.
fn own_style(element: &BytesStart<'_>) -> Style {
    Style {
        italic: attribute(element, "fontStyle")
            .is_some_and(|value| matches!(value.trim(), "italic" | "oblique")),
        bold: attribute(element, "fontWeight").is_some_and(|value| value.trim() == "bold"),
        underline: attribute(element, "textDecoration")
            .is_some_and(|value| value.split_whitespace().any(|token| token == "underline")),
    }
}

/// Cue text as `WebVTT` wants it: runs of white space are one space, and lines have none at their
/// ends. With `xml:space="preserve"` the text is left as written.
fn tidy(text: &str, preserve: bool) -> String {
    if preserve {
        return text.trim_matches('\n').to_owned();
    }
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let collapsed = line
            .split(' ')
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        lines.push(collapsed.join(" "));
    }
    // A break at the very start or end, or twice in a row, is not a line.
    lines.retain(|line| !line.is_empty());
    lines.join("\n")
}

/// A TTML time expression in milliseconds: a clock time (`hh:mm:ss`, `hh:mm:ss.fff`, or
/// `hh:mm:ss:ff` in frames) or an offset (`5s`, `1.5s`, `100ms`, `2m`, `1h`, `120f`, `3600t`).
fn parse_time(text: &str, timing: Timing) -> Option<u64> {
    let text = text.trim();
    if text.contains(':') {
        clock_time(text, timing)
    } else {
        offset_time(text, timing)
    }
}

fn clock_time(text: &str, timing: Timing) -> Option<u64> {
    let fields = text.split(':').collect::<Vec<_>>();
    let digits = |field: &str, at_least: usize, at_most: usize| -> Option<u64> {
        (field.bytes().all(|byte| byte.is_ascii_digit())
            && (at_least..=at_most).contains(&field.len()))
        .then(|| field.parse().ok())?
    };
    let (hours, minutes, seconds, frames) = match fields.as_slice() {
        [hours, minutes, seconds] => (hours, minutes, *seconds, None),
        [hours, minutes, seconds, frames] => (hours, minutes, *seconds, Some(*frames)),
        _ => return None,
    };
    let hours = digits(hours, 2, 10)?;
    let minutes = digits(minutes, 2, 2).filter(|minutes| *minutes < 60)?;
    let (whole, fraction) = seconds
        .split_once('.')
        .map_or((seconds, None), |(whole, fraction)| (whole, Some(fraction)));
    let whole = digits(whole, 2, 2).filter(|seconds| *seconds < 60)?;
    let mut milliseconds = hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(whole * 1000)?;
    if let Some(fraction) = fraction {
        // The digits after the point are a decimal fraction of a second.
        milliseconds = milliseconds.checked_add(scaled(&format!("0.{fraction}"), 1000, 1)?)?;
    }
    if let Some(frames) = frames {
        // `ff` or `ff.sub`: sub-frames are finer than a millisecond for no practical rate.
        let frames = digits(frames.split('.').next()?, 2, 10)?;
        milliseconds = milliseconds.checked_add(frame_time(frames, timing)?)?;
    }
    Some(milliseconds)
}

fn offset_time(text: &str, timing: Timing) -> Option<u64> {
    let (number, metric) = ["ms", "h", "m", "s", "f", "t"]
        .iter()
        .find_map(|metric| text.strip_suffix(metric).map(|number| (number, *metric)))?;
    match metric {
        "h" => scaled(number, 3_600_000, 1),
        "m" => scaled(number, 60_000, 1),
        "s" => scaled(number, 1000, 1),
        "ms" => scaled(number, 1, 1),
        "f" => scaled(number, 1000 * timing.frames.1, timing.frames.0),
        _ => scaled(number, 1000, timing.ticks_per_second),
    }
}

fn frame_time(frames: u64, timing: Timing) -> Option<u64> {
    let numerator = u128::from(frames) * 1000 * u128::from(timing.frames.1);
    let denominator = u128::from(timing.frames.0);
    u64::try_from((numerator + denominator / 2) / denominator).ok()
}

/// `number` (digits with an optional decimal fraction) times `numerator / denominator`, rounded to
/// the nearest whole number.
fn scaled(number: &str, numerator: u64, denominator: u64) -> Option<u64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    // Nine fraction digits are plenty: a millisecond is three.
    let fraction = &fraction[..fraction.len().min(9)];
    let places = u32::try_from(fraction.len()).ok()?;
    let unit = 10u128.pow(places);
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u128>().ok()?
    };
    let value = whole.checked_mul(unit)?
        + if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u128>().ok()?
        };
    let numerator = value.checked_mul(u128::from(numerator))?;
    let denominator = unit.checked_mul(u128::from(denominator))?;
    u64::try_from(numerator.checked_add(denominator / 2)? / denominator).ok()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    const HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml" xmlns:tts="http://www.w3.org/ns/ttml#styling" xmlns:ttp="http://www.w3.org/ns/ttml#parameter" xmlns:xml="http://www.w3.org/XML/1998/namespace""#;

    fn document(attributes: &str, content: &str) -> String {
        format!("{HEAD} {attributes}>\n{content}\n</tt>")
    }

    /// `(start, end, text)` of every cue.
    fn cues(attributes: &str, content: &str) -> Vec<(u64, u64, String)> {
        ttml_cues(&document(attributes, content))
            .expect("valid")
            .into_iter()
            .map(|cue| (cue.start, cue.end, cue.text))
            .collect()
    }

    fn one(attributes: &str, body: &str) -> (u64, u64, String) {
        let mut found = cues(attributes, &format!("<body><div>{body}</div></body>"));
        assert_eq!(found.len(), 1, "{found:?}");
        found.remove(0)
    }

    fn refused(attributes: &str, content: &str) -> &'static str {
        ttml_cues(&document(attributes, content))
            .err()
            .expect("refused")
    }

    #[test]
    fn clock_times_and_offsets_in_every_form() {
        for (begin, end, expected) in [
            ("00:00:01.000", "00:00:02.500", (1000, 2500)),
            ("00:00:01.5", "00:00:02", (1500, 2000)),
            ("01:02:03.004", "01:02:04.000", (3_723_004, 3_724_000)),
            ("5s", "6.25s", (5000, 6250)),
            ("100ms", "1500ms", (100, 1500)),
            ("2m", "3m", (120_000, 180_000)),
            ("0.5h", "1h", (1_800_000, 3_600_000)),
        ] {
            let found = one("", &format!(r#"<p begin="{begin}" end="{end}">x</p>"#));
            assert_eq!((found.0, found.1), expected, "{begin} {end}");
        }
    }

    #[test]
    fn frames_follow_the_frame_rate_and_its_multiplier() {
        let frames = |attributes: &str, begin: &str| {
            one(
                attributes,
                &format!(r#"<p begin="{begin}" end="10s">x</p>"#),
            )
            .0
        };
        // 30 frames a second unless the document says otherwise.
        assert_eq!(frames("", "30f"), 1000);
        assert_eq!(frames("", "15f"), 500);
        assert_eq!(frames("", "00:00:01:15"), 1500);
        assert_eq!(frames(r#"ttp:frameRate="25""#, "25f"), 1000);
        assert_eq!(frames(r#"ttp:frameRate="25""#, "00:00:00:05"), 200);
        // 30 x 1000/1001 = 29.97: thirty frames last 1001 ms.
        let ntsc = r#"ttp:frameRate="30" ttp:frameRateMultiplier="1000 1001""#;
        assert_eq!(frames(ntsc, "30f"), 1001);
        assert_eq!(frames(ntsc, "00:00:00:30"), 1001);
    }

    #[test]
    fn ticks_follow_the_tick_rate() {
        let at = |attributes: &str, begin: &str| {
            one(
                attributes,
                &format!(r#"<p begin="{begin}" end="100s">x</p>"#),
            )
            .0
        };
        assert_eq!(at(r#"ttp:tickRate="10000000""#, "10000000t"), 1000);
        assert_eq!(at(r#"ttp:tickRate="10000000""#, "25000000t"), 2500);
        assert_eq!(at(r#"ttp:tickRate="1000""#, "2500t"), 2500);
        // With a frame rate and no tick rate, a tick is a frame (times the sub-frame rate).
        assert_eq!(at(r#"ttp:frameRate="25""#, "50t"), 2000);
    }

    #[test]
    fn times_count_from_the_start_of_the_parent() {
        let found = cues(
            "",
            r#"<body><div begin="10s" end="20s">
                 <p begin="1s" end="2s">a</p>
                 <p begin="2s" dur="3s">b</p>
                 <p begin="4s">c</p>
               </div></body>"#,
        );

        assert_eq!(
            found,
            [
                (11_000, 12_000, "a".to_owned()),
                (12_000, 15_000, "b".to_owned()),
                // No end of its own: it lasts as long as its parent, which ends at 20 s.
                (14_000, 20_000, "c".to_owned()),
            ]
        );
    }

    #[test]
    fn text_has_its_lines_and_spaces_tidied_and_its_markup_escaped() {
        let text = |body: &str| one("", &format!(r#"<p begin="0s" end="1s">{body}</p>"#)).2;
        assert_eq!(text("Hello<br/>there"), "Hello\nthere");
        assert_eq!(text("  one \n     two   <br/>  three  "), "one two\nthree");
        assert_eq!(text("a <span>b</span> c"), "a b c");
        assert_eq!(
            text("Tom &amp; Jerry &lt;3 &#169; &#x41;"),
            "Tom &amp; Jerry &lt;3 © A"
        );
        assert_eq!(text("<![CDATA[a < b & c]]>"), "a &lt; b &amp; c");
        // A break at the edges, or two in a row, makes no empty line.
        assert_eq!(text("<br/>x<br/><br/>y<br/>"), "x\ny");
        // xml:space="preserve" keeps what was written.
        let preserved = one(
            "",
            "<p begin=\"0s\" end=\"1s\" xml:space=\"preserve\">a  b\nc</p>",
        );
        assert_eq!(preserved.2, "a  b\nc");
    }

    #[test]
    fn italic_bold_and_underline_come_from_the_element_or_a_referenced_style() {
        let styled = r##"
            <head><styling>
              <style xml:id="it" tts:fontStyle="italic"/>
              <style xml:id="bd" tts:fontWeight="bold"/>
              <style xml:id="both" style="it bd" tts:textDecoration="underline"/>
              <style xml:id="loop" style="loop"/>
              <style xml:id="red" tts:color="#ff0000" tts:fontFamily="monospace"/>
            </styling></head>
            <body><div>
              <p begin="0s" end="1s" style="it">referenced</p>
              <p begin="1s" end="2s">plain <span tts:fontStyle="italic">span</span> and <span tts:fontWeight="bold" tts:textDecoration="underline">two</span></p>
              <p begin="2s" end="3s" style="both">chain</p>
              <p begin="3s" end="4s" style="it"><span style="it">no double</span> <span style="bd">inner</span></p>
              <p begin="4s" end="5s" style="loop red">no effect</p>
            </div><div style="bd"><p begin="5s" end="6s">inherited</p></div></body>"##;

        let found = cues("", styled);

        let texts = found.iter().map(|cue| cue.2.as_str()).collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "<i>referenced</i>",
                "plain <i>span</i> and <b><u>two</u></b>",
                "<i><b><u>chain</u></b></i>",
                "<i>no double <b>inner</b></i>",
                "no effect",
                "<b>inherited</b>",
            ]
        );
    }

    #[test]
    fn namespaces_decide_what_is_ttml() {
        let paragraph = |prefix: &str| format!(r#"<{prefix}p begin="0s" end="1s">x</{prefix}p>"#);
        // A prefixed root, and the older DFXP namespace.
        let prefixed = format!(
            r#"<tt:tt xmlns:tt="http://www.w3.org/ns/ttml"><tt:body><tt:div>{}</tt:div></tt:body></tt:tt>"#,
            paragraph("tt:")
        );
        assert_eq!(ttml_cues(&prefixed).unwrap().len(), 1);
        let dfxp = format!(
            r#"<tt xmlns="http://www.w3.org/2006/10/ttaf1"><body><div>{}</div></body></tt>"#,
            paragraph("")
        );
        assert_eq!(ttml_cues(&dfxp).unwrap().len(), 1);
        // No namespaces at all is read as TTML; a `p` in some other namespace is not a paragraph.
        let bare = format!("<tt><body><div>{}</div></body></tt>", paragraph(""));
        assert_eq!(ttml_cues(&bare).unwrap().len(), 1);
        let foreign = format!(
            r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><x:p xmlns:x="urn:other" begin="0s" end="1s">x</x:p>{}</div></body></tt>"#,
            paragraph("")
        );
        assert_eq!(ttml_cues(&foreign).unwrap().len(), 1);
    }

    #[test]
    fn head_and_metadata_text_is_not_a_caption() {
        let found = cues(
            "",
            r#"<head><metadata><ttm:title xmlns:ttm="http://www.w3.org/ns/ttml#metadata">A title</ttm:title></metadata>
               <layout><region xml:id="r1" tts:origin="10% 10%"/></layout></head>
               <body region="r1"><div><p begin="0s" end="1s" region="r1">caption</p></div></body>"#,
        );

        assert_eq!(found, [(0, 1000, "caption".to_owned())]);
    }

    #[test]
    fn cues_that_could_never_show_are_dropped() {
        let found = cues(
            "",
            r#"<body><div>
                 <p begin="5s" end="6s">late</p>
                 <p begin="1s" end="2s">early</p>
                 <p begin="3s" end="3s">zero length</p>
                 <p begin="4s" end="3s">backwards</p>
                 <p begin="7s" end="8s">   </p>
                 <p begin="9s" end="10s"><br/></p>
               </div></body>"#,
        );

        let texts = found.iter().map(|cue| cue.2.as_str()).collect::<Vec<_>>();
        assert_eq!(texts, ["late", "early"], "{found:?}");
    }

    #[test]
    fn what_cannot_be_read_is_refused_with_a_reason() {
        let p = |attributes: &str| format!("<body><div><p {attributes}>x</p></div></body>");
        assert!(
            refused(r#"ttp:timeBase="smpte""#, &p(r#"begin="0s" end="1s""#)).contains("time base")
        );
        assert!(
            refused(r#"ttp:timeBase="clock""#, &p(r#"begin="0s" end="1s""#)).contains("time base")
        );
        assert!(refused("", &p(r#"begin="0s""#)).contains("no end time"));
        for bad in [
            "1x",
            "00:61:00.000",
            "00:00:61.000",
            "abc",
            "1:2:3",
            "-1s",
            "1e3s",
            ".s",
            "00:00:01:5",
        ] {
            let reason = refused("", &p(&format!(r#"begin="{bad}" end="9s""#)));
            assert!(reason.contains("time expression"), "{bad}: {reason}");
        }
        assert!(
            refused(
                "",
                &p(r#"begin="0s" end="1s""#)
                    .replace("<p ", "<p><p ")
                    .replace("</p>", "</p></p>")
            )
            .contains("paragraph")
        );
        // XML itself: an entity the document does not define, and a document that is cut off.
        assert!(
            refused("", &p(r#"begin="0s" end="1s""#).replace('x', "&nbsp;")).contains("entity")
        );
        assert_eq!(
            ttml_cues("<tt><body><div><p begin=\"0s\" end=\"1s\">x</p>").err(),
            Some("is not well-formed XML")
        );
        assert_eq!(
            ttml_cues("<tt><body></div></tt>").err(),
            Some("is not well-formed XML")
        );
        // A DOCTYPE is never accepted, which also rules out entity tricks.
        let doctype = "<!DOCTYPE tt [<!ENTITY a \"aaaa\">]><tt><body/></tt>";
        assert!(ttml_cues(doctype).err().unwrap().contains("DOCTYPE"));
        // A frame rate of zero would divide by it.
        assert!(refused(r#"ttp:frameRate="0""#, "<body/>").contains("frame rate"));
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!(
            "<tt><body>{}<p begin=\"0s\" end=\"1s\">x</p>{}</body></tt>",
            "<div>".repeat(200),
            "</div>".repeat(200)
        );

        assert!(
            ttml_cues(&deep)
                .err()
                .unwrap()
                .contains("nested too deeply")
        );
    }

    /// Time grows with the size of the file and not faster: eight times the cues take about eight
    /// times as long (a quadratic converter would take sixty-four).
    #[test]
    fn conversion_time_grows_in_proportion_to_the_file() {
        let timed = |count: u64| {
            let mut body = String::from("<body><div>");
            for cue in 0..count {
                write!(
                    body,
                    r#"<p begin="{}ms" end="{}ms">line <span tts:fontStyle="italic">{cue}</span><br/>two</p>"#,
                    cue * 1500,
                    cue * 1500 + 1000
                )
                .unwrap();
            }
            body.push_str("</div></body>");
            let document = document("", &body);
            let started = std::time::Instant::now();
            let found = ttml_cues(&document).unwrap();
            assert_eq!(found.len() as u64, count);
            started.elapsed()
        };
        // Warm up, then take the best of a few runs of each size.
        timed(500);
        let small = (0..3).map(|_| timed(3_000)).min().unwrap();
        let large = (0..3).map(|_| timed(24_000)).min().unwrap();

        assert!(
            large < small * 24 + std::time::Duration::from_millis(100),
            "3,000 cues took {small:?} and 24,000 took {large:?}"
        );
    }

    #[test]
    fn detection_looks_only_at_the_root() {
        assert!(looks_like_ttml("<tt xmlns=\"x\"></tt>"));
        assert!(looks_like_ttml(
            "\u{feff}\n<?xml version=\"1.0\"?>\n<!-- c -->\n<tt:tt xmlns:tt=\"x\"/>"
        ));
        for not in [
            "WEBVTT\n",
            "<html/>",
            "<ttx/>",
            "<?xml version=\"1.0\"?><svg/>",
            "1\n00:00:01,000 --> 00:00:02,000\nx",
            "<!DOCTYPE tt><tt/>",
            "",
        ] {
            assert!(!looks_like_ttml(not), "{not:?}");
        }
    }
}
