//! Black-box protocol conformance checks against the running server.
//!
//! These start the real binary and audit its HLS and DASH output against the structural rules
//! of RFC 8216, ISO/IEC 23009-1, and ISO BMFF that players depend on: playlist grammar and
//! cross-references, declared durations against the durations inside the fragments, timeline
//! continuity, keyframe-aligned segment starts, `moof`/`mdat` offset arithmetic, and byte-for-byte
//! agreement between the HLS and DASH routes. When FFprobe is installed, the reassembled tracks
//! are also decoded and their frame counts compared with the source.
//!
//! This is not a substitute for the vendor validators (Apple's Media Streaming Validator, DASH-IF
//! conformance); those remain a release-time step. See `docs/conformance.md`.

#![allow(
    // Harness code: measurement math and byte parsing cast freely, and long linear scripts
    // read better than split ones. The production crate keeps the strict pedantic lints.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::format_push_string,
    clippy::large_futures,
    clippy::redundant_closure_for_method_calls,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref
)]

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Asset ID, fixture file, and how many audio packets packaging drops from it. Only files with
/// an edit list lose any: the encoder-padding frame before the edit starts.
const FIXTURES: [(&str, &str, u64); 24] = [
    ("aac", "h264-aac.mp4", 0),
    ("moovlast", "h264-aac-moov-last.mp4", 0),
    ("videoonly", "h264-video-only.mp4", 0),
    ("stereo44", "h264-aac-44100-stereo.mp4", 0),
    ("variable", "h264-variable-timing.mp4", 0),
    ("edits", "h264-aac-default-edits.mp4", 1),
    ("delay", "h264-aac-audio-delay.mp4", 0),
    ("twoaudio", "h264-aac-two-audio.mp4", 0),
    ("anamorphic", "h264-aac-anamorphic.mp4", 0),
    ("quicktime", "h264-aac-quicktime.mov", 0),
    ("hevc", "hevc-aac.mp4", 0),
    ("vp9opus", "vp9-opus.mp4", 0),
    ("av1", "av1-aac.mp4", 0),
    ("ac3", "h264-ac3.mp4", 0),
    ("eac3", "h264-eac3.mp4", 0),
    ("flac", "h264-flac.mp4", 0),
    ("audioonly", "aac-only.m4a", 1),
    ("audiotwo", "aac-two-tracks-only.m4a", 0),
    ("frag", "h264-aac-fragmented.mp4", 0),
    ("fraglegacy", "h264-aac-fragmented-legacy.mp4", 0),
    ("fragcmaf", "h264-aac-fragmented-cmaf.mp4", 0),
    ("fragsidx", "h264-aac-fragmented-sidx.mp4", 0),
    ("fragoffset", "h264-aac-fragmented-offset.mp4", 0),
    ("fragneg", "h264-aac-fragmented-negative-cts.mp4", 0),
];

/// The video codec prefix and audio codec each fixture's master playlist must declare; `None`
/// means the fixture has no such track.
fn expected_codecs(asset: &str) -> (Option<&'static str>, Option<&'static str>) {
    match asset {
        "hevc" => (Some("hvc1."), Some("mp4a.40.2")),
        "vp9opus" => (Some("vp09."), Some("opus")),
        "av1" => (Some("av01."), Some("mp4a.40.2")),
        "ac3" => (Some("avc1."), Some("ac-3")),
        "eac3" => (Some("avc1."), Some("ec-3")),
        "flac" => (Some("avc1."), Some("fLaC")),
        "audioonly" | "audiotwo" => (None, Some("mp4a.40.2")),
        "videoonly" | "variable" => (Some("avc1."), None),
        _ => (Some("avc1."), Some("mp4a.40.2")),
    }
}

// ---------------------------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------------------------

struct Server {
    child: Child,
    address: SocketAddr,
    log: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A test that fails while talking to the server needs to know what the server saw:
        // a reset connection says only that the server went away.
        if std::thread::panicking() {
            let alive = self.child.try_wait().ok().flatten();
            let log = fs::read_to_string(&self.log).unwrap_or_default();
            let tail = log.lines().rev().take(15).collect::<Vec<_>>();
            eprintln!(
                "--- server at {} (exit status so far: {alive:?}), last log lines from {}:\n{}",
                self.address,
                self.log.display(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn start_server() -> Server {
    let directory = root().join("target/conformance");
    fs::create_dir_all(&directory).unwrap();
    // The server picks its own port and says which. Choosing one here and handing it over would
    // leave a gap in which another test, or anything else on the machine, could take it, and a
    // connect check would then be answered by the wrong server.
    let mut config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\nmedia_root = \"{}\"\n[packaging]\nsegment_duration_ms = 1000\n[logging]\nlevel = \"info\"\nformat = \"compact\"\n",
        root().join("tests/fixtures").display()
    );
    for (id, file, _) in FIXTURES {
        config.push_str(&format!("[assets.{id}]\npath = \"{file}\"\n"));
    }
    let name = format!(
        "{}-{}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-")
    );
    let config_path = directory.join(format!("vod-{name}.toml"));
    fs::write(&config_path, config).unwrap();
    let log_path = directory.join(format!("server-{name}.log"));

    // The server writes its log to stdout and its fatal errors to stderr; keep both.
    let log = fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_segmentor"))
        .args(["serve", "--config"])
        .arg(&config_path)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("server should start");

    // The server logs `service_ready` with its address once assets are loaded and the port is
    // bound, so seeing it means this process, and not another, owns the address.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "server exited with {status} before it was ready:\n{}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        if let Some(address) = ready_address(&log) {
            return Server {
                child,
                address,
                log: log_path,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!(
        "server did not become ready:\n{}",
        fs::read_to_string(&log_path).unwrap_or_default()
    );
}

/// The address in the `service_ready` log line, if the server has logged it.
fn ready_address(log: &str) -> Option<SocketAddr> {
    let line = log.lines().find(|line| line.contains("service_ready"))?;
    let start = line.find("listen.address=")? + "listen.address=".len();
    line[start..]
        .split(|c: char| c.is_whitespace() || c == '"')
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Response {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("body should be UTF-8")
    }
}

fn get(server: &Server, path: &str) -> Response {
    get_with(server, path, &[])
}

fn get_with(server: &Server, path: &str, extra: &[(&str, &str)]) -> Response {
    let mut stream = TcpStream::connect(server.address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: conformance\r\nConnection: close\r\n");
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response should contain a header block");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<HashMap<_, _>>();
    let body = raw[split + 4..].to_vec();
    Response {
        status,
        headers,
        body,
    }
}

fn get_ok(server: &Server, path: &str) -> Response {
    let response = get(server, path);
    assert_eq!(response.status, 200, "GET {path}");
    response
}

/// Resolves a playlist- or manifest-relative reference against the URL it was fetched from.
fn resolve(base: &str, reference: &str) -> String {
    let base_path = base.split('?').next().unwrap();
    let directory = &base_path[..=base_path.rfind('/').unwrap()];
    format!("{directory}{reference}")
}

// ---------------------------------------------------------------------------------------------
// ISO BMFF reading
// ---------------------------------------------------------------------------------------------

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn be64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes[..8].try_into().unwrap())
}

/// Splits a byte range into boxes, asserting that sizes tile it exactly.
fn boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        assert!(data.len() - offset >= 8, "truncated box header");
        let size = be32(&data[offset..]) as usize;
        let name: [u8; 4] = data[offset + 4..offset + 8].try_into().unwrap();
        assert!(size >= 8, "box {name:?} has size {size}");
        assert!(
            offset + size <= data.len(),
            "box {name:?} overruns its parent"
        );
        result.push((name, &data[offset + 8..offset + size]));
        offset += size;
    }
    result
}

fn child<'a>(children: &[([u8; 4], &'a [u8])], name: &[u8; 4]) -> &'a [u8] {
    children
        .iter()
        .find(|(candidate, _)| candidate == name)
        .unwrap_or_else(|| panic!("missing {} box", String::from_utf8_lossy(name)))
        .1
}

#[derive(Debug)]
struct Init {
    track_id: u32,
    timescale: u32,
}

fn parse_init(data: &[u8]) -> Init {
    let top = boxes(data);
    assert_eq!(&top[0].0, b"ftyp", "init must start with ftyp");
    let moov = boxes(child(&top, b"moov"));
    let trex = boxes(child(&moov, b"mvex"));
    assert!(
        trex.iter().any(|(name, _)| name == b"trex"),
        "init needs mvex/trex"
    );
    let traks = moov.iter().filter(|(name, _)| name == b"trak").count();
    assert_eq!(traks, 1, "each init segment carries exactly one track");
    let trak = boxes(child(&moov, b"trak"));

    let tkhd = child(&trak, b"tkhd");
    let track_id = if tkhd[0] == 1 {
        be32(&tkhd[20..])
    } else {
        be32(&tkhd[12..])
    };
    let mdia = boxes(child(&trak, b"mdia"));
    let mdhd = child(&mdia, b"mdhd");
    let timescale = if mdhd[0] == 1 {
        be32(&mdhd[20..])
    } else {
        be32(&mdhd[12..])
    };

    // The sample tables must be empty: samples live in fragments only.
    let minf = boxes(child(&mdia, b"minf"));
    let stbl = boxes(child(&minf, b"stbl"));
    let stsz = child(&stbl, b"stsz");
    assert_eq!(
        be32(&stsz[8..]),
        0,
        "init segment must not describe samples"
    );
    Init {
        track_id,
        timescale,
    }
}

#[derive(Debug)]
struct Fragment {
    sequence: u32,
    track_id: u32,
    decode_time: u64,
    durations: Vec<u32>,
    first_sample_is_sync: bool,
}

impl Fragment {
    fn duration(&self) -> u64 {
        self.durations.iter().map(|d| u64::from(*d)).sum()
    }
}

fn parse_fragment(data: &[u8]) -> Fragment {
    let top = boxes(data);
    let names = top.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    assert!(
        names == [*b"moof", *b"mdat"] || names == [*b"styp", *b"moof", *b"mdat"],
        "unexpected top-level boxes {names:?}"
    );
    let moof_payload = child(&top, b"moof");
    let mdat = child(&top, b"mdat");
    let moof = boxes(moof_payload);
    let sequence = be32(&child(&moof, b"mfhd")[4..]);
    let traf = boxes(child(&moof, b"traf"));

    let tfhd = child(&traf, b"tfhd");
    let tfhd_flags = be32(tfhd) & 0x00ff_ffff;
    let track_id = be32(&tfhd[4..]);
    assert_ne!(
        tfhd_flags & 0x0002_0000,
        0,
        "tfhd must use default-base-is-moof so data offsets are moof-relative"
    );

    let tfdt = child(&traf, b"tfdt");
    let decode_time = if tfdt[0] == 1 {
        be64(&tfdt[4..])
    } else {
        u64::from(be32(&tfdt[4..]))
    };

    let trun = child(&traf, b"trun");
    let flags = be32(trun) & 0x00ff_ffff;
    let count = be32(&trun[4..]) as usize;
    let mut cursor = 8;
    let data_offset = if flags & 0x1 != 0 {
        let value = i32::from_be_bytes(trun[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        Some(value)
    } else {
        None
    };
    if flags & 0x4 != 0 {
        cursor += 4;
    }
    let mut durations = Vec::new();
    let mut sizes = Vec::new();
    let mut first_flags = None;
    for index in 0..count {
        if flags & 0x100 != 0 {
            durations.push(be32(&trun[cursor..]));
            cursor += 4;
        }
        if flags & 0x200 != 0 {
            sizes.push(u64::from(be32(&trun[cursor..])));
            cursor += 4;
        }
        if flags & 0x400 != 0 {
            let sample_flags = be32(&trun[cursor..]);
            if index == 0 {
                first_flags = Some(sample_flags);
            }
            cursor += 4;
        }
        if flags & 0x800 != 0 {
            cursor += 4;
        }
    }
    assert_eq!(
        cursor,
        trun.len(),
        "trun length must match its declared fields"
    );

    // The first payload byte must be exactly where data_offset points: right after the mdat
    // box header, which follows the moof.
    let moof_size = (moof_payload.len() + 8) as i64;
    let expected_offset = moof_size + 8;
    assert_eq!(
        i64::from(data_offset.expect("trun must carry a data offset")),
        expected_offset,
        "trun data_offset must skip the moof and the mdat header"
    );
    assert_eq!(
        sizes.iter().sum::<u64>(),
        mdat.len() as u64,
        "sample sizes must tile the mdat payload"
    );
    Fragment {
        sequence,
        track_id,
        decode_time,
        durations,
        // "sample_is_non_sync_sample" is bit 16 of the sample flags.
        first_sample_is_sync: first_flags.is_none_or(|flags| flags & 0x0001_0000 == 0),
    }
}

// ---------------------------------------------------------------------------------------------
// Track audit shared by HLS and DASH
// ---------------------------------------------------------------------------------------------

struct Audited {
    init: Vec<u8>,
    segments: Vec<Vec<u8>>,
    /// Where the first segment starts, in track ticks; zero unless the file had an edit list.
    start_ticks: u64,
    /// The sum of the segment durations, not counting `start_ticks`.
    total_ticks: u64,
    timescale: u32,
}

/// Checks a track's init and media segments against the declared segment durations (in track
/// timescale ticks) and returns the raw bytes for further comparison.
fn audit_track(
    kind: &str,
    init: &[u8],
    segments: Vec<Vec<u8>>,
    declared_start: Option<u64>,
    declared_ticks: impl Fn(usize, u32) -> u64,
) -> Audited {
    let init_info = parse_init(init);
    // A track may start after zero when its file has an edit list. A manifest that states the
    // start (DASH) is checked against it; one that does not (HLS) is taken from the first segment.
    let start_ticks = declared_start
        .or_else(|| {
            segments
                .first()
                .map(|first| parse_fragment(first).decode_time)
        })
        .unwrap_or(0);
    let mut expected_start = start_ticks;
    for (index, bytes) in segments.iter().enumerate() {
        let fragment = parse_fragment(bytes);
        assert_eq!(
            fragment.track_id, init_info.track_id,
            "{kind} segment {index} track id"
        );
        assert_eq!(
            fragment.sequence as usize,
            index + 1,
            "{kind} segment {index} sequence"
        );
        assert_eq!(
            fragment.decode_time, expected_start,
            "{kind} segment {index} must start where the previous one ended"
        );
        if kind == "video" {
            assert!(
                fragment.first_sample_is_sync,
                "video segment {index} must begin on a keyframe"
            );
        }
        let declared = declared_ticks(index, init_info.timescale);
        let actual = fragment.duration();
        // Playlists carry millisecond precision; allow one millisecond of rounding.
        let tolerance = u64::from(init_info.timescale) / 1000 + 1;
        assert!(
            actual.abs_diff(declared) <= tolerance,
            "{kind} segment {index}: declared {declared} ticks, fragment holds {actual}"
        );
        expected_start += actual;
    }
    Audited {
        init: init.to_vec(),
        segments,
        start_ticks,
        total_ticks: expected_start - start_ticks,
        timescale: init_info.timescale,
    }
}

// ---------------------------------------------------------------------------------------------
// HLS
// ---------------------------------------------------------------------------------------------

fn attributes(line: &str) -> HashMap<String, String> {
    let list = line.split_once(':').map_or("", |(_, rest)| rest);
    let mut result = HashMap::new();
    let mut key = String::new();
    let mut value = String::new();
    let mut in_key = true;
    let mut quoted = false;
    for character in list.chars().chain(std::iter::once(',')) {
        match character {
            '=' if in_key => in_key = false,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                result.insert(std::mem::take(&mut key), std::mem::take(&mut value));
                in_key = true;
            }
            _ if in_key => key.push(character),
            _ => value.push(character),
        }
    }
    result
}

/// Audits one asset's HLS presentation and returns its audited tracks by name.
fn audit_hls(server: &Server, asset: &str) -> HashMap<String, Audited> {
    let master_url = format!("/hls/{asset}/master.m3u8");
    let master = get_ok(server, &master_url);
    assert_eq!(
        master.headers["content-type"],
        "application/vnd.apple.mpegurl"
    );
    let text = master.text();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[0], "#EXTM3U",
        "{asset}: playlist must start with #EXTM3U"
    );
    let version = lines
        .iter()
        .find_map(|line| line.strip_prefix("#EXT-X-VERSION:"))
        .expect("master needs EXT-X-VERSION")
        .parse::<u32>()
        .unwrap();
    assert!(
        version >= 6,
        "fMP4 with EXT-X-MAP requires protocol version 6 or higher"
    );

    let stream = lines
        .iter()
        .position(|line| line.starts_with("#EXT-X-STREAM-INF"))
        .expect("master needs a variant");
    let stream_attributes = attributes(lines[stream]);
    let bandwidth = stream_attributes["BANDWIDTH"].parse::<u64>().unwrap();
    let average = stream_attributes["AVERAGE-BANDWIDTH"]
        .parse::<u64>()
        .unwrap();
    assert!(
        bandwidth >= average,
        "BANDWIDTH is a peak and must not be below the average"
    );
    let (video_codec, audio_codec) = expected_codecs(asset);
    let codecs = &stream_attributes["CODECS"];
    match video_codec {
        Some(prefix) => {
            assert!(
                codecs.starts_with(prefix),
                "{asset}: video codec string in {codecs}"
            );
            assert!(stream_attributes["RESOLUTION"].contains('x'));
        }
        None => assert!(
            !stream_attributes.contains_key("RESOLUTION"),
            "{asset}: audio-only variants have no resolution"
        ),
    }
    if let Some(audio) = audio_codec {
        assert!(
            codecs.contains(audio),
            "{asset}: {audio} missing from {codecs}"
        );
    }

    // The variant's own URI names its track: `video`, or the audio track of an audio-only asset.
    let variant = lines[stream + 1];
    let mut targets = vec![(
        variant.split('/').next().unwrap().to_owned(),
        variant.to_owned(),
    )];
    let media_lines = lines
        .iter()
        .filter(|line| line.starts_with("#EXT-X-MEDIA"))
        .collect::<Vec<_>>();
    match stream_attributes.get("AUDIO") {
        Some(group) => {
            assert!(
                !media_lines.is_empty(),
                "AUDIO names a group with no renditions"
            );
            let mut defaults = 0;
            for line in &media_lines {
                let media = attributes(line);
                assert_eq!(
                    &media["GROUP-ID"], group,
                    "AUDIO group must reference an EXT-X-MEDIA"
                );
                assert_eq!(media["TYPE"], "AUDIO");
                defaults += usize::from(media["DEFAULT"] == "YES");
                // The track's name in its URL is also its DASH Representation ID.
                let name = media["URI"].split('/').next().unwrap().to_owned();
                // An audio-only variant may point at its default rendition's own playlist.
                if !targets.iter().any(|(existing, _)| *existing == name) {
                    targets.push((name, media["URI"].clone()));
                }
            }
            assert_eq!(defaults, 1, "exactly one rendition is the default");
        }
        None => assert!(
            media_lines.is_empty(),
            "audio rendition without a group reference"
        ),
    }

    let mut audited = HashMap::new();
    for (kind, reference) in targets {
        let playlist_url = resolve(&master_url, &reference);
        let playlist = get_ok(server, &playlist_url);
        let body = playlist.text();
        let lines = body.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "#EXTM3U");
        assert_eq!(
            lines.last(),
            Some(&"#EXT-X-ENDLIST"),
            "VOD playlist must end with ENDLIST"
        );
        assert!(lines.contains(&"#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(lines.contains(&"#EXT-X-INDEPENDENT-SEGMENTS"));
        let map = lines
            .iter()
            .find_map(|line| line.strip_prefix("#EXT-X-MAP:"))
            .expect("fMP4 playlist needs EXT-X-MAP");
        let init_reference = attributes(&format!("#X:{map}"))["URI"].clone();
        let init = get_ok(server, &resolve(&playlist_url, &init_reference)).body;

        let mut extinf = Vec::new();
        let mut segment_urls = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if let Some(duration) = line.strip_prefix("#EXTINF:") {
                let seconds = duration.trim_end_matches(',').parse::<f64>().unwrap();
                extinf.push(seconds);
                segment_urls.push(resolve(&playlist_url, lines[index + 1]));
            }
        }
        let target_duration = lines
            .iter()
            .find_map(|line| line.strip_prefix("#EXT-X-TARGETDURATION:"))
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(
            extinf
                .iter()
                .all(|duration| duration.round() <= target_duration),
            "every EXTINF rounded to whole seconds must not exceed TARGETDURATION"
        );

        let segments = segment_urls
            .iter()
            .map(|url| get_ok(server, url).body)
            .collect::<Vec<_>>();
        let audit = audit_track(&kind, &init, segments, None, |index, timescale| {
            (extinf[index] * f64::from(timescale)).round() as u64
        });
        audited.insert(kind, audit);
    }
    audited
}

// ---------------------------------------------------------------------------------------------
// DASH
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Tag {
    name: String,
    attributes: HashMap<String, String>,
}

/// A minimal XML tag reader, sufficient for the generated manifest.
fn tags(xml: &str) -> Vec<Tag> {
    let mut result = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let end = rest[start..].find('>').expect("unterminated tag") + start;
        let inner = rest[start + 1..end].trim_end_matches('/').trim();
        rest = &rest[end + 1..];
        if inner.starts_with('/') || inner.starts_with('?') {
            continue;
        }
        let (name, attribute_text) = inner.split_once(char::is_whitespace).unwrap_or((inner, ""));
        let mut attributes = HashMap::new();
        let mut text = attribute_text;
        while let Some(equals) = text.find('=') {
            let key = text[..equals].trim().to_owned();
            let after = &text[equals + 2..];
            let close = after.find('"').expect("unterminated attribute");
            attributes.insert(key, after[..close].to_owned());
            text = &after[close + 1..];
        }
        result.push(Tag {
            name: name.to_owned(),
            attributes,
        });
    }
    result
}

fn parse_iso_seconds(value: &str) -> f64 {
    value
        .strip_prefix("PT")
        .and_then(|rest| rest.strip_suffix('S'))
        .unwrap_or_else(|| panic!("unsupported duration {value}"))
        .parse()
        .unwrap()
}

fn audit_dash(server: &Server, asset: &str) -> HashMap<String, Audited> {
    let manifest_url = format!("/dash/{asset}/manifest.mpd");
    let manifest = get_ok(server, &manifest_url);
    assert_eq!(manifest.headers["content-type"], "application/dash+xml");
    let text = manifest.text();
    assert!(
        text.starts_with("<?xml"),
        "manifest must start with an XML declaration"
    );
    let all = tags(&text);
    let mpd = &all[0];
    assert_eq!(mpd.name, "MPD");
    assert_eq!(mpd.attributes["xmlns"], "urn:mpeg:dash:schema:mpd:2011");
    assert_eq!(mpd.attributes["type"], "static");
    assert!(mpd.attributes["profiles"].starts_with("urn:mpeg:dash:profile:"));
    let presentation = parse_iso_seconds(&mpd.attributes["mediaPresentationDuration"]);
    parse_iso_seconds(&mpd.attributes["minBufferTime"]);

    let mut audited = HashMap::new();
    let mut representation: Option<&Tag> = None;
    let mut index = 0;
    while index < all.len() {
        let tag = &all[index];
        if tag.name == "Representation" {
            representation = Some(tag);
            assert!(tag.attributes["bandwidth"].parse::<u64>().unwrap() > 0);
            assert!(!tag.attributes["codecs"].is_empty());
        }
        if tag.name == "SegmentTemplate" {
            let rep = representation.expect("SegmentTemplate must sit inside a Representation");
            let id = rep.attributes["id"].clone();
            let start_number = tag.attributes["startNumber"].parse::<usize>().unwrap();
            let entries = all[index + 1..]
                .iter()
                .take_while(|candidate| candidate.name != "Representation")
                .filter(|candidate| candidate.name == "S")
                .collect::<Vec<_>>();
            let durations = entries
                .iter()
                .map(|candidate| candidate.attributes["d"].parse::<u64>().unwrap())
                .collect::<Vec<_>>();
            assert!(
                entries[1..]
                    .iter()
                    .all(|entry| !entry.attributes.contains_key("t")),
                "only the first S may state t; the rest follow on"
            );
            let declared_start = entries[0]
                .attributes
                .get("t")
                .map(|t| t.parse::<u64>().unwrap());
            assert!(!durations.is_empty(), "SegmentTimeline must list segments");

            let init_url = resolve(
                &manifest_url,
                &tag.attributes["initialization"].replace("$RepresentationID$", &id),
            );
            let init = get_ok(server, &init_url).body;
            let segments = (0..durations.len())
                .map(|offset| {
                    let media = tag.attributes["media"]
                        .replace("$RepresentationID$", &id)
                        .replace("$Number$", &(start_number + offset).to_string());
                    get_ok(server, &resolve(&manifest_url, &media)).body
                })
                .collect::<Vec<_>>();
            let audit = audit_track(&id, &init, segments, declared_start, |segment, _| {
                durations[segment]
            });
            let seconds =
                (audit.start_ticks + audit.total_ticks) as f64 / f64::from(audit.timescale);
            assert!(
                seconds <= presentation + 0.002,
                "{id} timeline ({seconds}s) exceeds mediaPresentationDuration ({presentation}s)"
            );
            audited.insert(id, audit);
        }
        index += 1;
    }
    let longest = audited
        .values()
        .map(|track| (track.start_ticks + track.total_ticks) as f64 / f64::from(track.timescale))
        .fold(0.0, f64::max);
    assert!(
        (longest - presentation).abs() < 0.002,
        "mediaPresentationDuration {presentation}s must equal the longest track {longest}s"
    );
    audited
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[test]
fn hls_and_dash_conform_and_agree_for_every_fixture() {
    let server = start_server();
    for (asset, _, _) in FIXTURES {
        let hls = audit_hls(&server, asset);
        let dash = audit_dash(&server, asset);
        assert_eq!(
            hls.keys().collect::<std::collections::BTreeSet<_>>(),
            dash.keys().collect::<std::collections::BTreeSet<_>>(),
            "{asset}: HLS and DASH must expose the same tracks"
        );
        for (track, from_hls) in &hls {
            let from_dash = &dash[track];
            assert_eq!(
                from_hls.init, from_dash.init,
                "{asset}/{track}: init differs between protocols"
            );
            assert_eq!(
                from_hls.segments, from_dash.segments,
                "{asset}/{track}: segments differ between protocols"
            );
        }
    }
}

#[test]
fn media_objects_support_conditional_and_range_requests() {
    let server = start_server();
    let playlist = get_ok(&server, "/hls/aac/video/index.m3u8").text();
    let init_line = playlist
        .lines()
        .find(|line| line.starts_with("#EXT-X-MAP"))
        .unwrap();
    let init_url = resolve(
        "/hls/aac/video/index.m3u8",
        &attributes(&format!("#X:{}", &init_line["#EXT-X-MAP:".len()..]))["URI"],
    );

    let full = get_ok(&server, &init_url);
    assert_eq!(full.headers["accept-ranges"], "bytes");
    assert!(full.headers["cache-control"].contains("immutable"));
    let etag = full.headers["etag"].clone();

    let partial = get_with(&server, &init_url, &[("Range", "bytes=0-7")]);
    assert_eq!(partial.status, 206);
    assert_eq!(partial.body, full.body[..8]);
    assert_eq!(
        partial.headers["content-range"],
        format!("bytes 0-7/{}", full.body.len())
    );

    let suffix = get_with(&server, &init_url, &[("Range", "bytes=-4")]);
    assert_eq!(suffix.status, 206);
    assert_eq!(suffix.body, full.body[full.body.len() - 4..]);

    let unchanged = get_with(&server, &init_url, &[("If-None-Match", &etag)]);
    assert_eq!(unchanged.status, 304);
    assert!(unchanged.body.is_empty());

    // A stale If-Range validator must yield the complete representation.
    let stale = get_with(
        &server,
        &init_url,
        &[("Range", "bytes=0-7"), ("If-Range", "\"stale\"")],
    );
    assert_eq!(stale.status, 200);
    assert_eq!(stale.body, full.body);
}

/// Reassembles each track and lets FFprobe decode it, comparing frame counts with the source.
#[test]
fn reassembled_tracks_decode_with_the_source_frame_counts() {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let server = start_server();
    let directory = root().join("target/conformance/reassembled");
    fs::create_dir_all(&directory).unwrap();

    for (asset, file, dropped) in FIXTURES {
        let tracks = audit_hls(&server, asset);
        for (kind, track) in tracks {
            let path = directory.join(format!("{asset}-{kind}.mp4"));
            let mut bytes = track.init.clone();
            for segment in &track.segments {
                bytes.extend_from_slice(segment);
            }
            fs::write(&path, bytes).unwrap();

            // A reassembled file holds one stream; the source holds them all, in file order.
            let decoded = probe_frames(&path, if kind == "video" { "v:0" } else { "a:0" });
            let source = probe_frames(
                &root().join("tests/fixtures").join(file),
                &source_selector(&kind),
            );
            let expected = source
                - if kind.starts_with("audio") {
                    dropped
                } else {
                    0
                };
            assert_eq!(
                decoded, expected,
                "{asset}/{kind}: frame count after repackaging"
            );

            if kind == "video" {
                assert_eq!(
                    video_properties(&path),
                    video_properties(&root().join("tests/fixtures").join(file)),
                    "{asset}: aspect ratio and colour must survive repackaging"
                );
            }

            let errors = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-f", "null", "-"])
                .output();
            if let Ok(output) = errors {
                assert!(
                    output.stderr.is_empty(),
                    "{asset}/{kind} decode errors: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

/// The properties a player needs to render the picture correctly, as FFprobe reports them.
fn video_properties(path: &Path) -> String {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=sample_aspect_ratio,color_space,color_transfer,color_primaries",
        ])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The FFprobe stream selector for a track named `video` or `audio-N` in the source file.
fn source_selector(kind: &str) -> String {
    match kind.strip_prefix("audio-") {
        Some(number) => format!("a:{}", number.parse::<usize>().unwrap() - 1),
        None => "v:0".to_owned(),
    }
}

fn probe_frames(path: &Path, selector: &str) -> u64 {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-count_packets", "-select_streams", selector])
        .args(["-show_entries", "stream=nb_read_packets", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Some codecs (AC-3) make FFprobe append an empty field after the count.
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .split(',')
        .next()
        .unwrap_or_default()
        .parse()
        .unwrap_or_else(|_| panic!("unexpected ffprobe output for {}", path.display()))
}

/// The stream start times FFprobe reports for `input`, as `(video, audio)` in seconds.
fn start_times(input: &str) -> (f64, f64) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,start_time",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
        .expect("ffprobe should run");
    assert!(
        output.status.success(),
        "{input}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut video = None;
    let mut audio = None;
    // FFprobe separates programs with a blank line, and adds a third field for some inputs.
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.split(',');
        let (kind, start) = (
            fields.next().unwrap(),
            fields.next().expect("codec_type,start_time"),
        );
        let start = start.parse::<f64>().unwrap();
        match kind {
            "video" => video = video.or(Some(start)),
            "audio" => audio = audio.or(Some(start)),
            _ => {}
        }
    }
    (
        video.expect("a video stream"),
        audio.expect("an audio stream"),
    )
}

/// The gap between audio and video is what an edit list encodes, so it must survive packaging
/// exactly, even though both tracks move forward by the same small amount.
#[test]
fn audio_and_video_stay_in_sync_through_edit_lists() {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let server = start_server();
    for (asset, file, _) in FIXTURES {
        if ["videoonly", "variable", "twoaudio", "audioonly", "audiotwo"].contains(&asset) {
            continue;
        }
        let (source_video, source_audio) = start_times(
            &root()
                .join("tests/fixtures")
                .join(file)
                .display()
                .to_string(),
        );
        let gap_in_source = source_audio - source_video;
        for protocol in ["hls/{}/master.m3u8", "dash/{}/manifest.mpd"] {
            let url = format!(
                "http://{}/{}",
                server.address,
                protocol.replace("{}", asset)
            );
            let (video, audio) = start_times(&url);
            assert!(
                ((audio - video) - gap_in_source).abs() < 0.001,
                "{asset} over {protocol}: audio leads video by {:.4}s, the source by {gap_in_source:.4}s",
                audio - video
            );
        }
    }
}
