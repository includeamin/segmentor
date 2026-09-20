//! Measures the performance budgets from TDD 0001 against the real server.
//!
//! Run with `make bench` (or `cargo bench --bench budgets`). Pass `-- --enforce` to exit
//! non-zero when a budget is missed. Environment overrides:
//!
//! - `BUDGET_STREAMS` (default 1000) and `BUDGET_SECONDS` (default 10) size the concurrent
//!   streaming test.
//! - `BUDGET_WORKERS` (default 4) sets the Tokio worker threads, mirroring the four-core
//!   reference host. Client and server share one runtime, so results are conservative.
//!
//! A 60-minute asset is synthesized once into `target/bench/` by looping the committed fixture
//! with FFmpeg (stream copy, no re-encode). Without FFmpeg the long-asset checks are skipped.

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

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use segmentor::benchmarking::{BenchAsset, BenchServer, ServerOptions};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------------------------

struct Percentiles {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentiles(mut samples: Vec<Duration>) -> Percentiles {
    samples.sort_unstable();
    let at = |fraction: f64| {
        let index = ((samples.len() as f64 * fraction).ceil() as usize).clamp(1, samples.len()) - 1;
        samples[index].as_secs_f64() * 1000.0
    };
    Percentiles {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: samples
            .last()
            .map_or(0.0, |sample| sample.as_secs_f64() * 1000.0),
    }
}

#[derive(Default)]
struct Report {
    rows: Vec<(String, String, String, Option<bool>)>,
}

impl Report {
    fn budget(&mut self, name: &str, target: &str, measured: String, pass: bool) {
        self.rows
            .push((name.to_owned(), target.to_owned(), measured, Some(pass)));
    }

    fn info(&mut self, name: &str, measured: String) {
        self.rows
            .push((name.to_owned(), "-".to_owned(), measured, None));
    }

    fn print(&self) -> bool {
        let widths = [
            self.rows.iter().map(|row| row.0.len()).max().unwrap_or(0),
            self.rows.iter().map(|row| row.1.len()).max().unwrap_or(0),
            self.rows.iter().map(|row| row.2.len()).max().unwrap_or(0),
        ];
        println!();
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  result",
            "measurement",
            "budget",
            "measured",
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        );
        let mut all_pass = true;
        for (name, target, measured, pass) in &self.rows {
            let result = match pass {
                Some(true) => "PASS",
                Some(false) => {
                    all_pass = false;
                    "FAIL"
                }
                None => "info",
            };
            println!(
                "{name:<w0$}  {target:<w1$}  {measured:<w2$}  {result}",
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2]
            );
        }
        all_pass
    }
}

fn describe(p: &Percentiles) -> String {
    format!(
        "p50 {:.3} ms, p95 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
        p.p50, p.p95, p.p99, p.max
    )
}

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

/// A 60-minute H.264/AAC file made by stream-copying the 3-second fixture 1,200 times.
fn long_asset() -> Option<PathBuf> {
    let path = root().join("target/bench/long-60min.mp4");
    if path.exists() {
        return Some(path);
    }
    std::fs::create_dir_all(path.parent()?).ok()?;
    eprintln!("generating {} (one-time, stream copy)...", path.display());
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-stream_loop", "1199", "-i"])
        .arg(root().join("tests/fixtures/h264-aac.mp4"))
        .args(["-c", "copy", "-use_editlist", "0"])
        .arg(&path)
        .status()
        .ok()?;
    if status.success() {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&path);
        None
    }
}

// ---------------------------------------------------------------------------------------------
// A minimal keep-alive HTTP/1.1 client
// ---------------------------------------------------------------------------------------------

struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
}

struct Reply {
    status: u16,
    length: usize,
    body: Vec<u8>,
}

impl Client {
    async fn connect(address: std::net::SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect(address).await?;
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            buffer: Vec::with_capacity(16 * 1024),
        })
    }

    /// One request; the body is kept only when `keep` is set.
    async fn get(&mut self, path: &str, keep: bool) -> std::io::Result<Reply> {
        self.stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: bench\r\n\r\n").as_bytes())
            .await?;
        self.buffer.clear();
        let mut chunk = [0u8; 16 * 1024];
        let header_end = loop {
            if let Some(position) = self
                .buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                break position + 4;
            }
            let read = self.stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        };
        let head = String::from_utf8_lossy(&self.buffer[..header_end]).to_ascii_lowercase();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut received = self.buffer.len() - header_end;
        let mut body = if keep {
            self.buffer[header_end..].to_vec()
        } else {
            Vec::new()
        };
        while received < length {
            let read = self.stream.read(&mut chunk).await?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            received += read;
            if keep {
                body.extend_from_slice(&chunk[..read]);
            }
        }
        Ok(Reply {
            status,
            length,
            body,
        })
    }
}

fn metric(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
        .unwrap_or(0)
}

fn resident_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse()
                    .ok()
            })
        })
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------------------------

async fn startup(report: &mut Report, path: &Path) -> BenchAsset {
    let mut samples = Vec::new();
    let mut last = None;
    for iteration in 0..12 {
        let started = Instant::now();
        let asset = BenchAsset::load(path, 6000)
            .await
            .expect("long asset should load");
        let elapsed = started.elapsed();
        if iteration > 0 {
            samples.push(elapsed);
        } else {
            report.info(
                "startup: first (cold cache) load",
                format!("{:.1} ms", elapsed.as_secs_f64() * 1000.0),
            );
        }
        last = Some(asset);
    }
    let asset = last.unwrap();
    let stats = percentiles(samples);
    report.budget(
        "startup: warm load of a 60-minute asset",
        "p95 < 250 ms",
        describe(&stats),
        stats.p95 < 250.0,
    );
    report.info(
        "startup: asset size",
        format!(
            "{} segments, index {:.1} MiB",
            asset.segment_count(),
            asset.index_bytes() as f64 / 1_048_576.0
        ),
    );
    asset
}

fn segment_preparation(report: &mut Report, asset: &BenchAsset) {
    let count = asset.segment_count() as u32;
    let mut samples = Vec::new();
    for iteration in 0..3000u32 {
        let started = Instant::now();
        asset
            .prepare_video_segment((iteration * 7) % count)
            .expect("segment should prepare");
        samples.push(started.elapsed());
    }
    let stats = percentiles(samples);
    report.budget(
        "segment: warm 6 s header generation",
        "p95 < 10 ms",
        describe(&stats),
        stats.p95 < 10.0,
    );
}

async fn playlists_and_segments(report: &mut Report, server: &BenchServer) -> (String, usize) {
    let mut client = Client::connect(server.address()).await.unwrap();
    let master = client.get("/hls/asset/master.m3u8", true).await.unwrap();
    let text = String::from_utf8(master.body).unwrap();
    let version = text
        .split("?v=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_hexdigit()).next())
        .unwrap()
        .to_owned();

    for (label, path) in [
        ("HLS master playlist", "/hls/asset/master.m3u8".to_owned()),
        (
            "HLS video media playlist",
            format!("/hls/asset/video/index.m3u8?v={version}"),
        ),
        ("DASH manifest", "/dash/asset/manifest.mpd".to_owned()),
    ] {
        let mut samples = Vec::new();
        for _ in 0..3000 {
            let started = Instant::now();
            let reply = client.get(&path, false).await.unwrap();
            assert_eq!(reply.status, 200, "{path}");
            samples.push(started.elapsed());
        }
        let stats = percentiles(samples);
        report.budget(
            &format!("playlist: {label} (loopback, includes client)"),
            "p95 < 2 ms",
            describe(&stats),
            stats.p95 < 2.0,
        );
    }

    // Full segment transfers and read amplification.
    let before = metric(&server.metrics_text(), "vod_source_read_bytes_total");
    let mut samples = Vec::new();
    let mut body_bytes = 0usize;
    let segments = 300usize;
    for index in 0..segments {
        let path = format!(
            "/hls/asset/video/segments/{}/media.m4s?v={version}",
            index % 500
        );
        let started = Instant::now();
        let reply = client.get(&path, false).await.unwrap();
        assert_eq!(reply.status, 200);
        samples.push(started.elapsed());
        body_bytes += reply.length;
    }
    report.info(
        "segment: full 6 s transfer over loopback",
        describe(&percentiles(samples)),
    );
    let read = metric(&server.metrics_text(), "vod_source_read_bytes_total") - before;
    let excess = read.saturating_sub(body_bytes as u64) / segments as u64;
    report.budget(
        "segment: source bytes read beyond payload",
        "<= 256 KiB per segment",
        format!("{excess} B per segment ({read} read for {body_bytes} sent)"),
        excess <= 256 * 1024,
    );
    (version, 500)
}

async fn sustained_streams(report: &mut Report, server: &BenchServer, version: &str) {
    let streams = env_number("BUDGET_STREAMS", 1000) as usize;
    let seconds = env_number("BUDGET_SECONDS", 10);
    let idle_rss = resident_kib();
    let stop = Arc::new(AtomicBool::new(false));
    let ok = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    // Scheduling-delay canary: a task that should wake every millisecond.
    let canary_stop = Arc::clone(&stop);
    let canary = tokio::spawn(async move {
        let mut delays = Vec::new();
        while !canary_stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            tokio::time::sleep(Duration::from_millis(1)).await;
            delays.push(started.elapsed().saturating_sub(Duration::from_millis(1)));
        }
        delays
    });
    let rss_stop = Arc::clone(&stop);
    let rss = tokio::spawn(async move {
        let mut peak = 0;
        while !rss_stop.load(Ordering::Relaxed) {
            peak = peak.max(resident_kib());
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        peak
    });

    let mut tasks = Vec::new();
    for stream in 0..streams {
        let (address, version) = (server.address(), version.to_owned());
        let (stop, ok, failed, bytes) = (
            Arc::clone(&stop),
            Arc::clone(&ok),
            Arc::clone(&failed),
            Arc::clone(&bytes),
        );
        tasks.push(tokio::spawn(async move {
            let Ok(mut client) = Client::connect(address).await else {
                failed.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let mut index = stream * 37;
            while !stop.load(Ordering::Relaxed) {
                index = (index * 1_103_515_245 + 12_345) % 500;
                let path = format!("/hls/asset/video/segments/{index}/media.m4s?v={version}");
                match client.get(&path, false).await {
                    Ok(reply) if reply.status < 500 => {
                        ok.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(reply.length as u64, Ordering::Relaxed);
                    }
                    Ok(_) | Err(_) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        match Client::connect(address).await {
                            Ok(fresh) => client = fresh,
                            Err(_) => return,
                        }
                    }
                }
            }
        }));
    }
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    stop.store(true, Ordering::Relaxed);
    for task in tasks {
        let _ = task.await;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let delays = canary.await.unwrap();
    let peak_rss = rss.await.unwrap();

    let (ok, failed) = (ok.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
    let total = ok + failed;
    let failure_rate = failed as f64 / total.max(1) as f64 * 100.0;
    report.budget(
        &format!("concurrency: {streams} sustained streams, origin 5xx and errors"),
        "< 0.1 %",
        format!("{failed} of {total} requests ({failure_rate:.3} %)"),
        total > 0 && failure_rate < 0.1,
    );
    report.info(
        "concurrency: throughput",
        format!(
            "{:.0} req/s, {:.1} MiB/s",
            ok as f64 / elapsed,
            bytes.load(Ordering::Relaxed) as f64 / 1_048_576.0 / elapsed
        ),
    );
    let per_stream = peak_rss.saturating_sub(idle_rss) as f64 / streams as f64;
    report.budget(
        "memory: extra resident per streaming connection (includes bench client buffers)",
        "<= 512 KiB",
        format!("{per_stream:.0} KiB (idle {idle_rss} KiB, peak {peak_rss} KiB)"),
        per_stream <= 512.0,
    );
    report.info(
        "event loop: scheduling delay of a 1 ms canary under load",
        describe(&percentiles(delays)),
    );
    report.info(
        "server: rejected or shed under load",
        format!(
            "{} requests shed, {} connections rejected, {} segment queue timeouts",
            metric(&server.metrics_text(), "vod_http_requests_shed_total"),
            metric(
                &server.metrics_text(),
                "vod_http_connections_rejected_total"
            ),
            metric(&server.metrics_text(), "vod_segment_queue_timeouts_total"),
        ),
    );
}

fn main() {
    // `cargo bench` passes `--bench`; `cargo test --all-targets` runs this binary without it and
    // must not start a minute-long load test.
    if !std::env::args().any(|argument| argument == "--bench") {
        println!("budgets: skipped (run with `cargo bench --bench budgets`)");
        return;
    }
    let enforce = std::env::args().any(|argument| argument == "--enforce");
    let workers = env_number("BUDGET_WORKERS", 4) as usize;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();

    println!("segmentor performance budgets");
    println!(
        "host: {} logical CPUs, {workers} Tokio workers",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );

    let mut report = Report::default();
    match long_asset() {
        None => println!("skipping long-asset checks: FFmpeg is unavailable or failed"),
        Some(path) => {
            let asset = runtime.block_on(startup(&mut report, &path));
            segment_preparation(&mut report, &asset);
            runtime.block_on(async {
                let server = BenchServer::start(&path, ServerOptions::default())
                    .await
                    .expect("server should start");
                let (version, _) = playlists_and_segments(&mut report, &server).await;
                sustained_streams(&mut report, &server, &version).await;
                server.stop().await;
            });
        }
    }

    let passed = report.print();
    if enforce && !passed {
        eprintln!("\nat least one budget was missed");
        std::process::exit(1);
    }
}
