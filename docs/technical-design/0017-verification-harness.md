# TDD 0017: Verification harness

- Status: Draft
- Created: 2026-10-16
- Updated: 2026-10-16
- Related ADRs: None
- Related designs: [TDD 0016](0016-adaptive-clips.md) (the feature that exposed the gap), [TDD 0008](0008-clipping-and-concatenation.md), [TDD 0009](0009-common-encryption-and-drm.md); [conformance](../conformance.md) (what is covered today); the roadmap's Phase 4 and "Gap review"

## Summary

Three layers of evidence, each answering a different question, built in this order:

1. **Mapper-driven conformance** (Rust, automated, in CI): the existing structural audit run against a server that gets its assets from a real mapper, so adaptive ladders, sequences and adaptive clips are audited out of process. Today the audit cannot see them.
2. **A browser playback suite** (Playwright with hls.js and dash.js, automated, in a CI workflow of its own): proves that a player starts, seeks, plays to the end and switches quality across a clip boundary.
3. **Vendor validators** (Apple's Media Stream Validator, the DASH-IF conformance tool): a scripted, recorded release check, not CI, because of what the tools allow.

## Context

- **What exists.** `tests/conformance.rs` starts the real binary and audits what a player fetches, but builds its server from a static catalog of single files (`[assets.x] path = ...`), so it cannot express renditions or clips. `examples/mapper` is an out-of-process mapper that forwards `location`, `renditions`, `clips`, `subtitles` and `encryption` from a JSON catalog unchanged, and takes its port from `PORT`. `demo/index.html` already plays assets with hls.js and dash.js, loaded from a CDN. [conformance.md](../conformance.md) lists the vendor validators and the browser tests as "not run".
- **Why it matters now.** The review of TDD 0016 found an audio desynchronisation that every test passed, because nothing played the result. The structural audit and the in-process decode tests prove the bytes are well formed; they do not prove a player can use them.
- **What the tools allow** (checked on 2026-10-16; each point is from the tool's own pages, not tried here unless stated):
  - **Apple's `mediastreamvalidator`** ships in the HTTP Live Streaming Tools, a download that needs an Apple Developer account. Apple documents it for macOS; a third party reports a CentOS 7 package, which Apple does not confirm. It cannot be fetched by a workflow, so it cannot be a CI step.
  - **The DASH-IF conformance software** is open source (PHP/Laravel with a queue worker, and a Node frontend; a `Dockerfile` and `compose.yaml` are in the repository). Its README does not document a command-line or non-interactive mode, so whether it can run unattended is **unverified**. A hosted validator exists, and it keeps uploaded files for up to 24 hours.
  - **Playwright's bundled Chromium has no H.264 or AAC**, so the suite must use the Google Chrome channel (`npx playwright install chrome`), which a GitHub runner can install. Chrome supports the Clear Key EME key system.
  - **Node 26 and npm 12 are installed** on the development machine; `conformance.md` says they were not where the suite was first written.

## Goals

- Every kind of asset the server can serve is audited out of process, through a real mapper: single file, ladder, sequence, adaptive clips, subtitles, Clear Key.
- A player test that fails when a stream does not start, seek, play to the end, or switch quality at a clip boundary, and that is run on every change in CI.
- A release check for the two vendor validators that anyone can follow, whose results are recorded in the repository with the tool version, the vodnik version and the date.
- Each layer is mutation-checked: a known-bad stream makes it fail.

## Non-goals

- Certification. Passing these proves what they check and nothing else; `conformance.md` keeps saying so.
- A device and player matrix (Safari, tvOS, ExoPlayer, smart TVs). Safari native HLS is a manual line in the release check.
- Load testing; that is a separate roadmap item.
- Real Widevine, FairPlay and PlayReady playback, which need an operator's licence server; Clear Key is the only DRM system a browser test can use.

## Design

### Layer 1: mapper-driven conformance

`tests/conformance.rs` gains `start_server_with_mapper(catalog)`: it starts `python3 examples/mapper/mapper.py` on a free port (`PORT=0` is not supported by the script, so the port is chosen by the test and passed in, with the same ready-line wait as the server), then the binary with `resolver.http` pointing at it. Assets come from a harness catalog, `tests/fixtures/harness-catalog.json`, kept apart from the demo's `catalog.json` so the demo can change freely.

The audit gains the rules a grid needs: per-clip `EXT-X-MAP` and discontinuity in HLS, one `Period` per clip in DASH with the same representation IDs in each, segment numbers continuous across clips, and every `video-{id}` rendition ending where the others do. Assets: the rendition ladder with its two audio languages, the `preroll` sequence, an adaptive-clips grid cut mid-GOP (the case that failed), a trimmed ladder, and `drm-clearkey`.

### Layer 2: browser suite

A Node project under `tests/browser/` with a lockfile, using `@playwright/test`, `hls.js` and `dash.js` at pinned versions **installed from npm, not a CDN**, so a run does not depend on the network. A small page serves both players; a global setup starts the mapper and the binary on free ports. The cases, per player and asset:

- **starts:** `currentTime` advances past 0.5 s;
- **seeks:** to the middle and to a second before the end, and playback resumes;
- **plays to the end:** `ended` fires and the played ranges cover the duration;
- **no fatal player error** and no failed media request;
- **adaptive clips:** quality is forced from one level to another at the clip boundary (`hls.currentLevel`, dash.js's quality setter) and playback continues;
- **Clear Key:** the encrypted asset plays through EME.

It runs on the Chrome channel in `.github/workflows/browser.yml`, on pull requests and on `main`. It is **not a required check at first**: browser tests are slower and flakier than the rest, and the workflow should earn that status by passing consistently.

### Layer 3: vendor validators

`make release-check` starts a server with the harness catalog and prints, for each asset, the master playlist and manifest URLs and the exact commands for the two tools. The person running the release pastes the results into a table in `conformance.md` (tool and version, vodnik version, date, asset, result). Apple's validator runs on a Mac against the printed URL. For the DASH-IF tool the first step is a **spike** to find out whether its compose file can be run locally and driven without a browser; if it can, it becomes a script, and if not it stays a documented manual step using the hosted validator on a manifest that is public only for the run.

## Security and limits

- The suite starts local processes on loopback only, with `allow_insecure_mapper` set for the test mapper, as the dev compose file does.
- The hosted DASH-IF validator is third-party: only the fixture-based harness assets are ever sent to it, never customer media, and the check says so.
- The browser workflow takes no secrets and has `contents: read`.

## Observability

Playwright's trace, video and the server log are kept as workflow artifacts on a failure. The conformance suite already keeps the server log under `target/conformance`.

## Testing

Each layer is mutation-checked once, by hand, and the mutation recorded in `conformance.md`: Layer 1 against the existing mutation (fragment sequence numbers starting at 2) and a new one (an adaptive-clips grid whose audio rendition is cut from `from_ms` instead of the video's span, which is the bug the TDD 0016 review found); Layer 2 against the same two, plus a stream whose second period is missing a representation.

## Rollout

1. **Stage 1, mapper-driven conformance.** The harness catalog, `start_server_with_mapper`, the grid rules, and the mutation checks. No new tooling.
2. **Stage 2, the browser suite.** The Node project, the player page, the cases, and `browser.yml`, first as a non-required check.
3. **Stage 3, the release check.** `make release-check`, the results table, and the DASH-IF spike.

## Open questions

- **Is a non-required browser workflow acceptable at first**, or should it block merges from the start?
- **Is a manual Apple check enough?** It cannot be automated without Apple's licence terms allowing it; the alternative is a self-hosted Mac runner, which is a cost and a maintenance burden for one tool.
- **The DASH-IF spike's outcome.** If the tool cannot run unattended, the release check relies on the hosted validator; confirm that sending the fixture-based assets there is acceptable.
