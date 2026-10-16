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
    clippy::struct_field_names,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    reason = "harness code, not the production crate"
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
    /// The mapper this server gets its assets from, when it has one; dropped (and killed) with it.
    _mapper: Option<MapperProcess>,
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

/// The server with audio as its own HLS rendition: the audits below check every stream as a
/// single track and compare HLS with DASH byte for byte, and DASH never muxes.
fn start_server() -> Server {
    start_server_with(false)
}

/// The server with `packaging.hls_mux_audio` set as given; true is the shipped default.
fn start_server_with(hls_mux_audio: bool) -> Server {
    // The server picks its own port and says which. Choosing one here and handing it over would
    // leave a gap in which another test, or anything else on the machine, could take it, and a
    // connect check would then be answered by the wrong server.
    let mut config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\nmedia_root = \"{}\"\n[packaging]\nsegment_duration_ms = 1000\nhls_mux_audio = {hls_mux_audio}\n[logging]\nlevel = \"info\"\nformat = \"compact\"\n",
        root().join("tests/fixtures").display()
    );
    let name_suffix = if hls_mux_audio { "-muxed" } else { "" };
    for (id, file, _) in FIXTURES {
        config.push_str(&format!("[assets.{id}]\npath = \"{file}\"\n"));
    }
    launch(&config, name_suffix, None)
}

/// A mapper process, killed when dropped.
struct MapperProcess {
    child: Child,
    log: PathBuf,
}

impl Drop for MapperProcess {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = fs::read_to_string(&self.log).unwrap_or_default();
            eprintln!(
                "--- mapper, last log lines from {}:\n{}",
                self.log.display(),
                log
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The server, getting its assets from the example mapper (`examples/mapper`) serving
/// `tests/fixtures/harness-catalog.json`, so that assets a static catalog cannot express (renditions
/// and clips) can be audited out of process.
fn start_server_with_mapper() -> Server {
    let directory = root().join("target/conformance");
    fs::create_dir_all(&directory).unwrap();
    let name = unique_name();
    let log_path = directory.join(format!("mapper-{name}.log"));
    let log = fs::File::create(&log_path).unwrap();
    // `PORT=0` makes the mapper pick a free port and print it, for the same reason the server does.
    let child = Command::new("python3")
        .arg(root().join("examples/mapper/mapper.py"))
        .env("PORT", "0")
        .env(
            "CATALOG_PATH",
            root().join("tests/fixtures/harness-catalog.json"),
        )
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .expect("python3 should start the example mapper");
    let mut mapper = MapperProcess {
        child,
        log: log_path.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let port = loop {
        if let Some(status) = mapper.child.try_wait().unwrap() {
            panic!(
                "the mapper exited with {status} before it was ready:\n{}",
                fs::read_to_string(&log_path).unwrap_or_default()
            );
        }
        let text = fs::read_to_string(&log_path).unwrap_or_default();
        if let Some(port) = text
            .lines()
            .find(|line| line.starts_with("mapper: serving "))
            .and_then(|line| line.rsplit_once(':'))
            .and_then(|(_, port)| port.trim().parse::<u16>().ok())
        {
            break port;
        }
        assert!(
            Instant::now() < deadline,
            "the mapper did not report a port:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let config = format!(
        "[server]\nlisten = \"127.0.0.1:0\"\n[storage]\nmedia_root = \"{}\"\n[packaging]\nsegment_duration_ms = 1000\nhls_mux_audio = false\n[logging]\nlevel = \"info\"\nformat = \"compact\"\n[resolver]\ntype = \"http\"\n[resolver.http]\nbase_url = \"http://127.0.0.1:{port}\"\nallow_insecure_mapper = true\n",
        root().join("tests/fixtures").display()
    );
    launch(&config, "-mapper", Some(mapper))
}

/// A name for this test's files that no other test shares.
fn unique_name() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-")
    )
}

/// Writes `config`, starts the binary on it, and waits until it says it is ready.
fn launch(config: &str, name_suffix: &str, mapper: Option<MapperProcess>) -> Server {
    let directory = root().join("target/conformance");
    fs::create_dir_all(&directory).unwrap();
    let name = unique_name() + name_suffix;
    let config_path = directory.join(format!("vod-{name}.toml"));
    fs::write(&config_path, config).unwrap();
    let log_path = directory.join(format!("server-{name}.log"));

    // The server writes its log to stdout and its fatal errors to stderr; keep both.
    let log = fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vodnik"))
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
                _mapper: mapper,
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
    let data_offset = (flags & 0x1 != 0).then(|| {
        let value = i32::from_be_bytes(trun[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        value
    });
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
/// timescale ticks) and returns the raw bytes for further comparison. `first_sequence` is the
/// `mfhd` number the first segment must carry: 1 for a whole presentation, and the clip's first
/// global segment number plus one for a clip of a sequence.
fn audit_track(
    kind: &str,
    init: &[u8],
    segments: Vec<Vec<u8>>,
    declared_start: Option<u64>,
    first_sequence: u32,
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
            first_sequence as usize + index,
            "{kind} segment {index} sequence"
        );
        assert_eq!(
            fragment.decode_time, expected_start,
            "{kind} segment {index} must start where the previous one ended"
        );
        if kind.starts_with("video") {
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
        let audit = audit_track(&kind, &init, segments, None, 1, |index, timescale| {
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
            assert_ne!(tag.attributes["codecs"], "");
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
            let audit = audit_track(&id, &init, segments, declared_start, 1, |segment, _| {
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

/// The payload of a fragment's `mdat`.
fn mdat(fragment: &[u8]) -> &[u8] {
    child(&boxes(fragment), b"mdat")
}

/// TDD 0011: where a fixture has video and audio, the default HLS stream is muxed, and each of
/// its fragments carries exactly the separate video fragment's samples followed by the first
/// audio track's, with a `traf` for each (audio only while it lasts).
#[test]
fn muxed_fragments_carry_the_separate_streams_samples_for_every_fixture() {
    let muxed = start_server_with(true);
    let separate = start_server();
    let mut assets = 0;
    for (asset, _, _) in FIXTURES {
        let (video, audio) = expected_codecs(asset);
        let master = get_ok(&muxed, &format!("/hls/{asset}/master.m3u8")).text();
        let has_both = video.is_some() && audio.is_some();
        assert_eq!(
            master.contains("\nmuxed/index.m3u8?v="),
            has_both,
            "{asset}: {master}"
        );
        if !has_both {
            continue;
        }
        let playlist_path = format!("/hls/{asset}/muxed/index.m3u8");
        let playlist = get_ok(&muxed, &playlist_path).text();
        let map = playlist
            .lines()
            .find_map(|line| line.strip_prefix("#EXT-X-MAP:URI=\""))
            .and_then(|rest| rest.split('"').next())
            .expect("muxed playlist names its init segment");
        let init = get_ok(&muxed, &resolve(&playlist_path, map)).body;
        let moov = boxes(child(&boxes(&init), b"moov"));
        let traks = moov.iter().filter(|(name, _)| name == b"trak").count();
        assert_eq!(traks, 2, "{asset}: the muxed init carries video and audio");

        for uri in playlist
            .lines()
            .filter(|line| line.starts_with("segments/"))
        {
            let path = resolve(&playlist_path, uri);
            let fragment = get_ok(&muxed, &path).body;
            let video = get_ok(&separate, &path.replace("/muxed/", "/video/")).body;
            let audio = get(&separate, &path.replace("/muxed/", "/audio-1/"));
            let mut expected = mdat(&video).to_vec();
            let mut expected_trafs = 1;
            if audio.status == 200 {
                expected.extend_from_slice(mdat(&audio.body));
                expected_trafs += 1;
            } else {
                assert_eq!(audio.status, 404, "{asset} {uri}: audio has ended");
            }
            assert_eq!(mdat(&fragment), &expected[..], "{asset} {uri}");
            let fragment_boxes = boxes(child(&boxes(&fragment), b"moof"));
            let found = fragment_boxes
                .iter()
                .filter(|(name, _)| name == b"traf")
                .count();
            assert_eq!(found, expected_trafs, "{asset} {uri}");
        }
        assets += 1;

        if Command::new("ffmpeg").arg("-version").output().is_ok() {
            let url = format!("http://{}/hls/{asset}/master.m3u8", muxed.address);
            let decoded = Command::new("ffmpeg")
                .args(["-v", "error", "-i", &url, "-f", "null", "-"])
                .output()
                .unwrap();
            assert!(
                decoded.status.success(),
                "{asset}: {}",
                String::from_utf8_lossy(&decoded.stderr)
            );
        }
    }
    // Every fixture except the audio-only and video-only ones.
    assert_eq!(assets, FIXTURES.len() - 4, "assets with a muxed stream");
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
    assert_eq!(unchanged.body, [] as [u8; 0]);

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

/// The I-frame playlist lists exactly the source's keyframes, and each listed fragment, with the
/// video init segment in front of it, decodes to one picture.
#[test]
fn every_iframe_fragment_decodes_to_exactly_one_frame() {
    let ffprobe = Command::new("ffprobe").arg("-version").output().is_ok();
    let server = start_server();
    let directory = root().join("target/conformance/iframes");
    fs::create_dir_all(&directory).unwrap();

    for (asset, file, _) in FIXTURES {
        let master = get_ok(&server, &format!("/hls/{asset}/master.m3u8")).text();
        let has_video = expected_codecs(asset).0.is_some();
        let declared = master
            .lines()
            .find(|line| line.starts_with("#EXT-X-I-FRAME-STREAM-INF"));
        assert_eq!(
            declared.is_some(),
            has_video,
            "{asset}: only assets with video have an I-frame stream\n{master}"
        );
        let Some(declared) = declared else {
            assert_eq!(
                get(&server, &format!("/hls/{asset}/video/iframes.m3u8")).status,
                404,
                "{asset}: no video, no I-frame playlist"
            );
            continue;
        };
        assert!(declared.contains("CODECS=\""), "{asset}: {declared}");
        assert!(declared.contains("BANDWIDTH="), "{asset}: {declared}");
        assert!(!declared.contains("mp4a"), "{asset}: video codec only");

        let playlist_url = format!("/hls/{asset}/video/iframes.m3u8");
        let playlist = get_ok(&server, &playlist_url);
        assert_eq!(
            playlist.headers["content-type"],
            "application/vnd.apple.mpegurl"
        );
        let text = playlist.text();
        assert!(text.contains("#EXT-X-I-FRAMES-ONLY"), "{asset}: {text}");
        assert!(text.ends_with("#EXT-X-ENDLIST\n"), "{asset}");
        let map = text
            .lines()
            .find_map(|line| line.strip_prefix("#EXT-X-MAP:URI=\""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("the playlist names the init segment");
        let init = get_ok(&server, &resolve(&playlist_url, map)).body;
        let entries = text
            .lines()
            .filter(|line| line.starts_with("iframes/"))
            .collect::<Vec<_>>();

        if ffprobe {
            assert_eq!(
                entries.len() as u64,
                source_keyframes(&root().join("tests/fixtures").join(file)),
                "{asset}: the playlist lists the source's keyframes"
            );
        }
        for (index, entry) in entries.iter().enumerate() {
            assert!(
                entry.starts_with(&format!("iframes/{index}/media.m4s?v=")),
                "{asset}: {entry}"
            );
            let fragment = get_ok(&server, &resolve(&playlist_url, entry));
            assert_eq!(fragment.headers["content-type"], "video/mp4");
            if !ffprobe || index % 4 != 0 {
                continue;
            }
            let path = directory.join(format!("{asset}-{index}.mp4"));
            let mut bytes = init.clone();
            bytes.extend_from_slice(&fragment.body);
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                probe_frames(&path, "v:0"),
                1,
                "{asset}: fragment {index} holds one picture"
            );
            let errors = Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(&path)
                .args(["-f", "null", "-"])
                .output();
            if let Ok(output) = errors {
                assert!(
                    output.stderr.is_empty(),
                    "{asset}: fragment {index} decode errors: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        let past_the_end = get(
            &server,
            &resolve(
                &playlist_url,
                &format!(
                    "iframes/{}/media.m4s?v={}",
                    entries.len(),
                    entries[0].rsplit('=').next().unwrap()
                ),
            ),
        );
        assert_eq!(past_the_end.status, 404, "{asset}: no such keyframe");
    }
}

/// The number of keyframes FFprobe finds in the first video stream.
fn source_keyframes(path: &Path) -> u64 {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "packet=flags", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe should run");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('K'))
        .count() as u64
}

#[test]
fn a_server_fed_by_a_mapper_serves_a_single_file() {
    let server = start_server_with_mapper();

    let master = get_ok(&server, "/hls/single/master.m3u8");

    assert!(master.text().starts_with("#EXTM3U"));
}

#[test]
fn a_track_audit_can_start_at_a_later_sequence_number() {
    let server = start_server();
    let tracks = audit_hls(&server, "aac");
    let video = &tracks["video"];
    let durations = video
        .segments
        .iter()
        .map(|segment| parse_fragment(segment).duration())
        .collect::<Vec<_>>();

    // A segment numbered from 1 passes when it is expected to start at 1.
    audit_track(
        "video",
        &video.init,
        video.segments.clone(),
        None,
        1,
        |index, _| durations[index],
    );

    // The same segments are refused when the clip they belong to would start at sequence 5.
    let refused = std::panic::catch_unwind(|| {
        audit_track(
            "video",
            &video.init,
            video.segments.clone(),
            None,
            5,
            |index, _| durations[index],
        );
    });
    assert!(refused.is_err(), "a wrong first sequence number must fail");
}

// ---------------------------------------------------------------------------------------------
// Presentations of several clips and renditions, through a mapper (TDD 0017)
// ---------------------------------------------------------------------------------------------

/// An asset of the harness catalog: the track names a player sees in the URLs and representation
/// IDs, how many clips each track has, and, for each clip, the least a video clip must hold, which
/// is the window it was asked for less what a video clip may lose to the frames shown late.
struct MapperAsset {
    id: &'static str,
    tracks: &'static [&'static str],
    clips: usize,
    min_video_ms: &'static [u64],
}

const MAPPER_ASSETS: [MapperAsset; 5] = [
    MapperAsset {
        id: "single",
        tracks: &["video", "audio-1"],
        clips: 1,
        min_video_ms: &[3000],
    },
    MapperAsset {
        id: "ladder",
        tracks: &["video-720p", "video-480p", "audio-1", "audio-2"],
        clips: 1,
        min_video_ms: &[3000],
    },
    MapperAsset {
        id: "preroll",
        tracks: &["video", "audio-1"],
        clips: 2,
        min_video_ms: &[3000, 1500],
    },
    MapperAsset {
        id: "grid",
        tracks: &["video-720p", "video-480p", "audio-1"],
        clips: 2,
        min_video_ms: &[900, 2000],
    },
    MapperAsset {
        id: "trimmed",
        tracks: &["video-720p", "video-480p", "audio-1"],
        clips: 1,
        min_video_ms: &[1900],
    },
];

fn audit_mapper_asset(server: &Server, asset: &MapperAsset) {
    let hls = audit_hls_clips(server, asset.id);
    let dash = audit_dash_clips(server, asset.id);
    let mut expected = asset.tracks.to_vec();
    expected.sort_unstable();
    let mut found = hls.keys().map(String::as_str).collect::<Vec<_>>();
    found.sort_unstable();
    assert_eq!(found, expected, "{}: the tracks a player sees", asset.id);
    for (track, audits) in &hls {
        assert_eq!(
            audits.len(),
            asset.clips,
            "{}/{track}: clips in HLS",
            asset.id
        );
    }
    check_presentation(asset, &hls, &dash);
}

#[test]
fn single_ladder_preroll_and_trimmed_assets_conform() {
    let server = start_server_with_mapper();
    for asset in &MAPPER_ASSETS {
        if asset.id != "grid" {
            audit_mapper_asset(&server, asset);
        }
    }
}

/// Clips cut at 1000 ms start before the keyframe a player would call the second one, because a
/// video clip begins on the keyframe at or before its window; the audio has to follow the video.
#[test]
fn a_grid_cut_mid_gop_conforms() {
    let server = start_server_with_mapper();
    let grid = MAPPER_ASSETS
        .iter()
        .find(|asset| asset.id == "grid")
        .unwrap();
    audit_mapper_asset(&server, grid);
}

/// FFmpeg cannot judge a sequence as one stream (its HLS demuxer keeps the first init segment's
/// settings across an `EXT-X-MAP` change), so every clip of every track is reassembled from its own
/// init segment and segments and decoded on its own, as the in-process tests do.
#[test]
fn every_clip_of_every_mapper_asset_decodes() {
    if Command::new("ffprobe").arg("-version").output().is_err() {
        eprintln!("skipping: ffprobe is not installed");
        return;
    }
    let server = start_server_with_mapper();
    let directory = root().join("target/conformance/mapper-reassembled");
    fs::create_dir_all(&directory).unwrap();
    let mut decoded = 0;
    for asset in &MAPPER_ASSETS {
        for (track, clips) in audit_hls_clips(&server, asset.id) {
            for (position, clip) in clips.iter().enumerate() {
                let path = directory.join(format!("{}-{track}-{position}.mp4", asset.id));
                let mut bytes = clip.audited.init.clone();
                for segment in &clip.audited.segments {
                    bytes.extend_from_slice(segment);
                }
                fs::write(&path, bytes).unwrap();
                let output = Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&path)
                    .args(["-f", "null", "-"])
                    .output()
                    .expect("ffmpeg should run next to ffprobe");
                assert!(
                    output.stderr.is_empty(),
                    "{}/{track} clip {position} decode errors: {}",
                    asset.id,
                    String::from_utf8_lossy(&output.stderr)
                );
                let selector = if track.starts_with("video") {
                    "v:0"
                } else {
                    "a:0"
                };
                assert!(
                    probe_frames(&path, selector) > 0,
                    "{}/{track} clip {position} decodes to nothing",
                    asset.id
                );
                decoded += 1;
            }
        }
    }
    // single 2 tracks x 1 clip, ladder 4 x 1, preroll 2 x 2, grid 3 x 2, trimmed 3 x 1.
    assert_eq!(decoded, 2 + 4 + 4 + 6 + 3);
}

/// One clip of one track, audited.
struct ClipAudit {
    audited: Audited,
    /// The global number of the clip's first segment.
    first_segment: u32,
    /// Where the clip starts on the presentation timeline, and how long it is, in milliseconds.
    start_ms: u64,
    length_ms: u64,
}

fn clip_audit(audited: Audited, first_segment: u32) -> ClipAudit {
    let scale = u64::from(audited.timescale);
    ClipAudit {
        first_segment,
        start_ms: audited.start_ticks * 1000 / scale,
        length_ms: audited.total_ticks * 1000 / scale,
        audited,
    }
}

/// A clip as the rules between tracks see it, in milliseconds.
struct ClipShape {
    start_ms: u64,
    length_ms: u64,
    /// The first sample's duration, rounded up: the grain a clip can be cut at.
    sample_ms: u64,
    segment_starts_ms: Vec<u64>,
}

impl ClipAudit {
    fn shape(&self) -> ClipShape {
        let timescale = u64::from(self.audited.timescale);
        let first = parse_fragment(&self.audited.segments[0]);
        ClipShape {
            start_ms: self.start_ms,
            length_ms: self.length_ms,
            sample_ms: (u64::from(first.durations[0]) * 1000).div_ceil(timescale),
            segment_starts_ms: self
                .audited
                .segments
                .iter()
                .map(|segment| parse_fragment(segment).decode_time * 1000 / timescale)
                .collect(),
        }
    }
}

fn shapes(clips: &[ClipAudit]) -> Vec<ClipShape> {
    clips.iter().map(ClipAudit::shape).collect()
}

/// The number in a `segments/{n}/media.m4s` reference.
fn segment_number(reference: &str) -> u32 {
    reference
        .split("segments/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .and_then(|number| number.parse().ok())
        .unwrap_or_else(|| panic!("no segment number in {reference}"))
}

/// The init reference of clip `position` of `clips`: a presentation of one clip has a plain init
/// segment, and one of several has an init segment for each (TDD 0008). The reference is relative
/// to the playlist or manifest that holds it, so a DASH one also carries the track's directory.
fn assert_init_reference(context: &str, init: &str, position: usize, clips: usize) {
    if clips == 1 {
        assert!(
            init.contains("init.mp4") && !init.contains("clips/"),
            "{context}: a single clip has a plain init reference, not {init}"
        );
    } else {
        assert!(
            init.contains(&format!("clips/{position}/init.mp4")),
            "{context}: init reference {init} should name clip {position}"
        );
    }
}

/// One clip of a media playlist: its init reference, and each segment's reference and duration.
struct PlaylistClip {
    init: String,
    segments: Vec<(String, f64)>,
}

/// Splits a VOD media playlist into clips at its `EXT-X-MAP`s, refusing one that is not well
/// formed: a discontinuity precedes every clip but the first, and nothing else.
fn parse_media_playlist(context: &str, body: &str) -> Vec<PlaylistClip> {
    let lines = body.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "#EXTM3U", "{context}");
    assert_eq!(lines.last(), Some(&"#EXT-X-ENDLIST"), "{context}");
    assert!(lines.contains(&"#EXT-X-PLAYLIST-TYPE:VOD"), "{context}");
    assert!(lines.contains(&"#EXT-X-INDEPENDENT-SEGMENTS"), "{context}");
    let target = lines
        .iter()
        .find_map(|line| line.strip_prefix("#EXT-X-TARGETDURATION:"))
        .unwrap()
        .parse::<f64>()
        .unwrap();
    let mut clips: Vec<PlaylistClip> = Vec::new();
    let mut discontinuity = false;
    for (index, line) in lines.iter().enumerate() {
        if *line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            assert_eq!(
                discontinuity,
                !clips.is_empty(),
                "{context}: a discontinuity precedes every clip but the first, and nothing else"
            );
            discontinuity = false;
            clips.push(PlaylistClip {
                init: attributes(&format!("#X:{map}"))["URI"].clone(),
                segments: Vec::new(),
            });
        } else if let Some(duration) = line.strip_prefix("#EXTINF:") {
            assert!(
                !discontinuity,
                "{context}: a discontinuity inside a clip, before segment {}",
                lines[index + 1]
            );
            let seconds = duration.trim_end_matches(',').parse::<f64>().unwrap();
            assert!(
                seconds.round() <= target,
                "{context}: EXTINF within TARGETDURATION"
            );
            clips
                .last_mut()
                .expect("a map comes before any segment")
                .segments
                .push((lines[index + 1].to_owned(), seconds));
        }
    }
    assert!(!discontinuity, "{context}: a discontinuity before nothing");
    assert!(!clips.is_empty(), "{context}: no clips");
    clips
}

/// Audits one asset's HLS presentation clip by clip: every media playlist of the master (the
/// variants and the audio renditions), each split at its `EXT-X-MAP`s.
fn audit_hls_clips(server: &Server, asset: &str) -> HashMap<String, Vec<ClipAudit>> {
    let master_url = format!("/hls/{asset}/master.m3u8");
    let master = get_ok(server, &master_url).text();
    let lines = master.lines().collect::<Vec<_>>();
    assert_eq!(lines[0], "#EXTM3U", "{asset}");
    let mut targets: Vec<(String, String)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let reference = if line.starts_with("#EXT-X-STREAM-INF") {
            let stream = attributes(line);
            let bandwidth = stream["BANDWIDTH"].parse::<u64>().unwrap();
            let average = stream["AVERAGE-BANDWIDTH"].parse::<u64>().unwrap();
            assert!(bandwidth >= average, "{asset}: BANDWIDTH is a peak");
            assert_ne!(stream["CODECS"], "", "{asset}: a variant names its codecs");
            lines[index + 1].to_owned()
        } else if line.starts_with("#EXT-X-MEDIA") {
            let media = attributes(line);
            assert_eq!(media["TYPE"], "AUDIO", "{asset}");
            media["URI"].clone()
        } else {
            continue;
        };
        let name = reference.split('/').next().unwrap().to_owned();
        if !targets.iter().any(|(existing, _)| *existing == name) {
            targets.push((name, reference));
        }
    }
    assert!(
        !targets.is_empty(),
        "{asset}: the master lists no playlists"
    );

    let mut result = HashMap::new();
    for (name, reference) in targets {
        let playlist_url = resolve(&master_url, &reference);
        let context = format!("{asset}/{name}");
        let clips = parse_media_playlist(&context, &get_ok(server, &playlist_url).text());

        let mut audits = Vec::new();
        let mut next_segment = None;
        for (position, clip) in clips.iter().enumerate() {
            assert!(
                !clip.segments.is_empty(),
                "{context}: clip {position} has no segments"
            );
            assert_init_reference(&context, &clip.init, position, clips.len());
            let first_segment = segment_number(&clip.segments[0].0);
            if let Some(expected) = next_segment {
                assert_eq!(
                    first_segment, expected,
                    "{context}: segment numbers continue across clips"
                );
            }
            for (offset, (uri, _)) in clip.segments.iter().enumerate() {
                assert_eq!(
                    segment_number(uri),
                    first_segment + u32::try_from(offset).unwrap(),
                    "{context}: clip {position} numbers its segments in order"
                );
            }
            next_segment = Some(first_segment + u32::try_from(clip.segments.len()).unwrap());
            let init = get_ok(server, &resolve(&playlist_url, &clip.init)).body;
            let bytes = clip
                .segments
                .iter()
                .map(|(uri, _)| get_ok(server, &resolve(&playlist_url, uri)).body)
                .collect::<Vec<_>>();
            let audited = audit_track(
                &name,
                &init,
                bytes,
                None,
                first_segment + 1,
                |index, timescale| (clip.segments[index].1 * f64::from(timescale)).round() as u64,
            );
            audits.push(clip_audit(audited, first_segment));
        }
        result.insert(name, audits);
    }
    result
}

/// Audits one asset's DASH presentation period by period: each `Period` is a clip, and every
/// `Representation` in it is audited the way a whole presentation's is.
fn audit_dash_clips(server: &Server, asset: &str) -> HashMap<String, Vec<ClipAudit>> {
    let manifest_url = format!("/dash/{asset}/manifest.mpd");
    let manifest = get_ok(server, &manifest_url);
    assert_eq!(manifest.headers["content-type"], "application/dash+xml");
    let all = tags(&manifest.text());
    let mpd = &all[0];
    assert_eq!(mpd.name, "MPD");
    assert_eq!(mpd.attributes["type"], "static");
    let presentation = parse_iso_seconds(&mpd.attributes["mediaPresentationDuration"]);
    let periods = all.iter().filter(|tag| tag.name == "Period").count();
    assert!(periods >= 1, "{asset}: a manifest has a period");

    let mut result: HashMap<String, Vec<ClipAudit>> = HashMap::new();
    let mut ids_by_period: Vec<Vec<String>> = Vec::new();
    let mut period_start_ms = 0;
    let mut representation: Option<&Tag> = None;
    for (index, tag) in all.iter().enumerate() {
        match tag.name.as_str() {
            "Period" => {
                period_start_ms =
                    (parse_iso_seconds(&tag.attributes["start"]) * 1000.0).round() as u64;
                ids_by_period.push(Vec::new());
            }
            "Representation" => {
                representation = Some(tag);
                ids_by_period
                    .last_mut()
                    .unwrap()
                    .push(tag.attributes["id"].clone());
            }
            "SegmentTemplate" => {
                let id = representation
                    .expect("a template sits in a representation")
                    .attributes["id"]
                    .clone();
                let position = ids_by_period.len() - 1;
                let context = format!("{asset}/{id} period {position}");
                let start_number = tag.attributes["startNumber"].parse::<u32>().unwrap();
                let entries = all[index + 1..]
                    .iter()
                    .take_while(|candidate| candidate.name != "Representation")
                    .filter(|candidate| candidate.name == "S")
                    .collect::<Vec<_>>();
                let durations = entries
                    .iter()
                    .map(|entry| entry.attributes["d"].parse::<u64>().unwrap())
                    .collect::<Vec<_>>();
                assert!(
                    !durations.is_empty(),
                    "{context}: a timeline lists segments"
                );
                let declared_start = entries[0]
                    .attributes
                    .get("t")
                    .map(|t| t.parse::<u64>().unwrap());
                let initialization =
                    tag.attributes["initialization"].replace("$RepresentationID$", &id);
                assert_init_reference(&context, &initialization, position, periods);
                let init = get_ok(server, &resolve(&manifest_url, &initialization)).body;
                let segments = (0..durations.len())
                    .map(|offset| {
                        let media = tag.attributes["media"]
                            .replace("$RepresentationID$", &id)
                            .replace("$Number$", &(start_number as usize + offset).to_string());
                        get_ok(server, &resolve(&manifest_url, &media)).body
                    })
                    .collect::<Vec<_>>();
                let audited = audit_track(
                    &id,
                    &init,
                    segments,
                    declared_start,
                    start_number + 1,
                    |segment, _| durations[segment],
                );
                let offset = tag.attributes.get("presentationTimeOffset");
                assert!(
                    offset.is_some() || periods == 1,
                    "{context}: a period of several needs a presentationTimeOffset"
                );
                if let Some(offset) = offset {
                    let offset = offset.parse::<u64>().unwrap();
                    // A video clip starts exactly at the offset; an audio clip starts at its first
                    // frame at or after it, which is at most one frame later.
                    let late = audited.start_ticks.checked_sub(offset).unwrap_or_else(|| {
                        panic!("{context}: the first fragment starts before the offset")
                    });
                    let frame = u64::from(parse_fragment(&audited.segments[0]).durations[0]);
                    assert!(
                        late <= frame,
                        "{context}: the first fragment starts {late} ticks after the offset, \
                         more than a sample ({frame} ticks)"
                    );
                    assert!(
                        (offset * 1000 / u64::from(audited.timescale)).abs_diff(period_start_ms)
                            <= 1,
                        "{context}: the offset is where the period starts"
                    );
                }
                result
                    .entry(id)
                    .or_default()
                    .push(clip_audit(audited, start_number));
            }
            _ => {}
        }
    }
    for (position, ids) in ids_by_period.iter().enumerate().skip(1) {
        let (mut first, mut here) = (ids_by_period[0].clone(), ids.clone());
        first.sort();
        here.sort();
        assert_eq!(
            first, here,
            "{asset}: period {position} has the representations of period 0"
        );
    }
    // KNOWN DEVIATION (docs/conformance.md): the duration of a sequence is where its video ends, and
    // the last clip keeps the audio that runs past it (TDD 0008), so an audio track can end after
    // `mediaPresentationDuration`: by 21 ms in `grid` and 66 ms in `preroll` today. A single file
    // states the longest track instead and is held to 2 ms by the audit above. The bound below is a
    // little over the worst case seen; it exists to catch a larger overrun, not to excuse this one.
    for (id, audits) in &result {
        let last = audits.last().unwrap();
        let end = (last.start_ms + last.length_ms) as f64 / 1000.0;
        assert!(
            end <= presentation + 0.1,
            "{asset}/{id}: ends at {end}s, past mediaPresentationDuration ({presentation}s)"
        );
    }
    result
}

/// What must hold between the tracks of one presentation, and between its two protocols.
fn check_presentation(
    asset: &MapperAsset,
    hls: &HashMap<String, Vec<ClipAudit>>,
    dash: &HashMap<String, Vec<ClipAudit>>,
) {
    let name = asset.id;
    assert_eq!(
        hls.keys().collect::<std::collections::BTreeSet<_>>(),
        dash.keys().collect::<std::collections::BTreeSet<_>>(),
        "{name}: HLS and DASH expose the same tracks"
    );
    for (track, clips) in hls {
        let other = &dash[track];
        assert_eq!(
            clips.len(),
            other.len(),
            "{name}/{track}: clips in HLS and DASH"
        );
        for (position, (a, b)) in clips.iter().zip(other).enumerate() {
            assert_eq!(
                a.audited.init, b.audited.init,
                "{name}/{track} clip {position}: init"
            );
            assert_eq!(
                a.audited.segments, b.audited.segments,
                "{name}/{track} clip {position}: segments differ between protocols"
            );
            assert_eq!(
                a.first_segment, b.first_segment,
                "{name}/{track} clip {position}"
            );
        }
        check_continuity(&format!("{name}/{track}"), &shapes(clips));
    }

    let mut videos = hls
        .iter()
        .filter(|(track, _)| track.starts_with("video"))
        .map(|(track, clips)| (track.as_str(), shapes(clips)))
        .collect::<Vec<_>>();
    videos.sort_by_key(|(track, _)| *track);
    let (reference_track, reference) = &videos[0];
    for (track, clips) in &videos {
        check_covers_windows(&format!("{name}/{track}"), clips, asset.min_video_ms);
    }
    for (track, clips) in &videos[1..] {
        check_renditions_alike(
            &format!("{name}: {track} against {reference_track}"),
            reference,
            clips,
        );
    }
    for (track, clips) in hls.iter().filter(|(track, _)| track.starts_with("audio")) {
        check_audio_follows_video(&format!("{name}/{track}"), reference, &shapes(clips));
    }
}

fn tolerance(a: &ClipShape, b: &ClipShape) -> u64 {
    a.sample_ms.max(b.sample_ms) + 1
}

/// A clip starts where the one before ended, to within the coarser clip's sample: the audio of a
/// clip may overlap the next by under a frame.
fn check_continuity(context: &str, clips: &[ClipShape]) {
    for (position, pair) in clips.windows(2).enumerate() {
        let end = pair[0].start_ms + pair[0].length_ms;
        assert!(
            end.abs_diff(pair[1].start_ms) <= tolerance(&pair[0], &pair[1]),
            "{context}: clip {position} ends at {end} ms and clip {} starts at {} ms",
            position + 1,
            pair[1].start_ms
        );
    }
}

/// Two video renditions are cut alike (TDD 0016): the same clips, the same segments starting at the
/// same instants, and the same start and length, each to within a sample. Segment starts agreeing
/// is not enough on its own, since a rendition whose clip ends early would drift every later one.
fn check_renditions_alike(context: &str, reference: &[ClipShape], other: &[ClipShape]) {
    assert_eq!(reference.len(), other.len(), "{context}: clips");
    for (position, (first, clip)) in reference.iter().zip(other).enumerate() {
        let context = format!("{context}, clip {position}");
        let slack = tolerance(first, clip);
        assert_eq!(
            first.segment_starts_ms.len(),
            clip.segment_starts_ms.len(),
            "{context}: segments"
        );
        for (index, (a, b)) in first
            .segment_starts_ms
            .iter()
            .zip(&clip.segment_starts_ms)
            .enumerate()
        {
            assert!(
                a.abs_diff(*b) <= slack,
                "{context}: segment {index} starts at {a} ms and at {b} ms"
            );
        }
        assert!(
            first.start_ms.abs_diff(clip.start_ms) <= slack,
            "{context}: starts at {} ms and at {} ms",
            first.start_ms,
            clip.start_ms
        );
        assert!(
            first.length_ms.abs_diff(clip.length_ms) <= slack,
            "{context}: is {} ms long and {} ms long",
            first.length_ms,
            clip.length_ms
        );
    }
}

/// How much audio the last clip may carry past the end of the video: the last clip keeps the audio
/// that runs past it (TDD 0008), which is 66 ms in `preroll` today.
const TRAILING_AUDIO_MS: u64 = 100;

/// An audio track has the video's clips. Each starts within one audio frame of the video's, and is
/// the video's length to within a video sample and an audio frame, which is what shows an audio
/// clip cut from a different instant of its source than the video's. Only the last clip may run
/// longer than that, by the trailing audio.
fn check_audio_follows_video(context: &str, video: &[ClipShape], audio: &[ClipShape]) {
    assert_eq!(video.len(), audio.len(), "{context}: clips");
    for (position, (v, a)) in video.iter().zip(audio).enumerate() {
        let context = format!("{context}, clip {position}");
        assert!(
            a.start_ms.abs_diff(v.start_ms) <= a.sample_ms + 1,
            "{context}: the audio starts at {} ms, the video at {} ms",
            a.start_ms,
            v.start_ms
        );
        let slack = v.sample_ms + a.sample_ms + 2;
        let trailing = if position + 1 == video.len() {
            TRAILING_AUDIO_MS
        } else {
            0
        };
        assert!(
            v.length_ms.saturating_sub(a.length_ms) <= slack
                && a.length_ms.saturating_sub(v.length_ms) <= slack + trailing,
            "{context}: the audio is {} ms long, the video {} ms",
            a.length_ms,
            v.length_ms
        );
    }
}

/// A video clip holds at least the window it was asked for. It may begin earlier, because a video
/// clip starts on the keyframe at or before its window, but never later or shorter.
fn check_covers_windows(context: &str, video: &[ClipShape], minimums: &[u64]) {
    assert_eq!(video.len(), minimums.len(), "{context}: clips");
    for (position, (clip, minimum)) in video.iter().zip(minimums).enumerate() {
        assert!(
            clip.length_ms + clip.sample_ms + 1 >= *minimum,
            "{context}: clip {position} is {} ms long, but its window holds at least {minimum} ms",
            clip.length_ms
        );
    }
}

fn shape(start_ms: u64, length_ms: u64, segment_starts_ms: &[u64]) -> ClipShape {
    ClipShape {
        start_ms,
        length_ms,
        sample_ms: 34,
        segment_starts_ms: segment_starts_ms.to_vec(),
    }
}

fn refuses(check: impl FnOnce() + std::panic::UnwindSafe) -> bool {
    std::panic::catch_unwind(check).is_err()
}

const TWO_CLIP_PLAYLIST: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n\
#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n\
#EXT-X-MAP:URI=\"clips/0/init.mp4?v=1\"\n#EXTINF:1.000,\nsegments/0/media.m4s?v=1\n\
#EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"clips/1/init.mp4?v=1\"\n#EXTINF:1.000,\n\
segments/1/media.m4s?v=1\n#EXT-X-ENDLIST\n";

#[test]
fn a_playlist_splits_into_its_clips_and_refuses_a_misplaced_discontinuity() {
    let clips = parse_media_playlist("good", TWO_CLIP_PLAYLIST);
    assert_eq!(clips.len(), 2);
    assert_eq!(clips[1].init, "clips/1/init.mp4?v=1");

    let without = TWO_CLIP_PLAYLIST.replace("#EXT-X-DISCONTINUITY\n", "");
    assert!(
        refuses(|| {
            parse_media_playlist("no discontinuity", &without);
        }),
        "a clip boundary needs a discontinuity"
    );
    let stray = TWO_CLIP_PLAYLIST.replace(
        "segments/0/media.m4s?v=1\n",
        "segments/0/media.m4s?v=1\n#EXT-X-DISCONTINUITY\n#EXTINF:1.000,\nsegments/1/media.m4s?v=1\n",
    );
    assert!(
        refuses(|| {
            parse_media_playlist("stray discontinuity", &stray);
        }),
        "a discontinuity inside a clip is refused"
    );
}

#[test]
fn clips_that_do_not_continue_are_refused() {
    check_continuity("good", &[shape(0, 1000, &[0]), shape(1000, 900, &[1000])]);
    let gap = [shape(0, 1000, &[0]), shape(1200, 900, &[1200])];
    assert!(refuses(|| check_continuity("gap", &gap)));
}

#[test]
fn renditions_cut_differently_are_refused() {
    let reference = [shape(0, 1000, &[0, 500]), shape(1000, 900, &[1000])];
    check_renditions_alike(
        "good",
        &reference,
        &[shape(0, 1010, &[0, 505]), shape(1000, 900, &[1000])],
    );
    let different_count = [shape(0, 1000, &[0]), shape(1000, 900, &[1000])];
    assert!(refuses(|| check_renditions_alike(
        "count",
        &reference,
        &different_count
    )));
    let drifting_segment = [shape(0, 1000, &[0, 650]), shape(1000, 900, &[1000])];
    assert!(refuses(|| check_renditions_alike(
        "segment",
        &reference,
        &drifting_segment
    )));
    let short_last_clip = [shape(0, 1000, &[0, 500]), shape(1000, 100, &[1000])];
    assert!(refuses(|| check_renditions_alike(
        "length",
        &reference,
        &short_last_clip
    )));
    let late_start = [shape(0, 1000, &[0, 500]), shape(1300, 900, &[1000])];
    assert!(refuses(|| check_renditions_alike(
        "start",
        &reference,
        &late_start
    )));
}

#[test]
fn audio_cut_from_another_instant_than_the_video_is_refused() {
    let video = [shape(0, 1000, &[0]), shape(1000, 2000, &[1000])];
    check_audio_follows_video(
        "good",
        &video,
        &[shape(0, 1010, &[0]), shape(1010, 2020, &[1010])],
    );
    let short_audio = [shape(0, 1000, &[0]), shape(1000, 1100, &[1000])];
    assert!(refuses(|| check_audio_follows_video(
        "length",
        &video,
        &short_audio
    )));
    let late_audio = [shape(0, 1000, &[0]), shape(1100, 2000, &[1100])];
    assert!(refuses(|| check_audio_follows_video(
        "start",
        &video,
        &late_audio
    )));

    // The last clip keeps the audio that runs past the video; an earlier clip may not.
    check_audio_follows_video(
        "trailing audio",
        &video,
        &[shape(0, 1010, &[0]), shape(1010, 2066, &[1010])],
    );
    let long_middle = [shape(0, 1066, &[0]), shape(1066, 2000, &[1066])];
    assert!(refuses(|| check_audio_follows_video(
        "middle",
        &video,
        &long_middle
    )));
    let long_last = [shape(0, 1010, &[0]), shape(1010, 2300, &[1010])];
    assert!(refuses(|| check_audio_follows_video(
        "last", &video, &long_last
    )));
}

#[test]
fn a_video_clip_shorter_than_its_window_is_refused() {
    check_covers_windows(
        "good",
        &[shape(0, 950, &[0]), shape(950, 3000, &[950])],
        &[900, 2000],
    );
    let short = [shape(0, 950, &[0]), shape(950, 1000, &[950])];
    assert!(refuses(|| check_covers_windows(
        "short",
        &short,
        &[900, 2000]
    )));
}
