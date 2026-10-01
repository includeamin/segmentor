# TDD 0009: Common encryption and DRM

- Status: Accepted; H.264 and audio implemented, HEVC pending
- Created: 2026-10-01
- Updated: 2026-10-01
- Related ADRs: [ADR 0001](../adr/0001-use-fragmented-mp4-for-media-segments.md) (fragmented MP4 segments)
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface this extends), [TDD 0006](0006-trick-play-subtitles-and-renditions.md) (its "DRM (later)" section recorded what this must not break), [TDD 0008](0008-clipping-and-concatenation.md) (sequences, which this covers)

## Summary

segmentor encrypts what it serves with Common Encryption (ISO/IEC 23001-7) in the `cbcs` scheme, and signals DRM systems in HLS and DASH, so one set of segments plays under Widevine, FairPlay, PlayReady, and Clear Key. The mapper supplies the content keys and each DRM system's data in an optional `encryption` object of its answer. segmentor never talks to a licence server: players fetch licences from the operator's DRM vendor, and segmentor only encrypts and signals where to go.

Encryption happens per request, like everything else segmentor serves. An encrypted segment is read into memory, encrypted, and returned; its output is deterministic, so immutable URLs, ETags, and CDN caching work unchanged. Assets without `encryption` are served byte for byte as today.

This is the first stage of DRM: the shared core. It deliberately leaves out AV1 and VP9, different keys per clip, key rotation, whole-segment HLS AES-128, encrypted source files, and a SPEKE adapter (see [Deferred](#deferred)).

## Context

Premium content cannot be licensed without studio-grade DRM, and nginx-vod-module offers DRM, so its absence is the largest remaining reason to choose vod-module over segmentor. `cbcs` is the one scheme every major DRM system accepts on both HLS and DASH, which is what lets one set of segments serve every player.

Two properties of the current pipeline shape this design:

- **Headers come from the index alone.** `fmp4::prepare_media_segment` builds a fragment's `moof` from the sample index, then the response streams sample bytes from the source without holding them. With `cbcs`, the `moof` must carry each video sample's subsample map (which bytes are clear and which encrypted), and that map comes from the NAL units inside the samples. An encrypted segment therefore has to be read before its header can be written.
- **Init segments copy the source's sample entry.** `fmp4::write_init_segment` copies `stsd` untouched and rewrites only header durations. Encryption renames the sample entry and nests a `sinf` inside it.

The mapper already carries per-asset secrets under HTTPS and a bearer token, has validation, TTLs, and caching, and is the component operators already write, so it is where content keys come from. Operators fetch keys from their DRM vendor inside their mapper.

## Goals

- One set of `cbcs` segments playable under Widevine, FairPlay, PlayReady, and Clear Key, across HLS and DASH, for single files, adaptive renditions, and sequences.
- Keys from the mapper, handled as secrets: never logged, never exposed, never written to disk.
- Deterministic encrypted output, so the URL and caching model is unchanged.
- No change of any kind for assets without `encryption`.
- Encryption verified by an independent implementation, not only by our own round trip.

## Non-goals

- Licence servers, key generation, or talking to a DRM vendor. The operator's mapper supplies keys and system data.
- The `cenc` (AES-CTR), `cbc1`, and `cens` schemes.
- AV1, VP9, Opus, and FLAC encryption in this stage.
- Different keys per clip, key rotation, whole-segment HLS AES-128, encrypted source passthrough, and SPEKE. See [Deferred](#deferred).

## Design

### Wire format

The mapper answer gains an optional `encryption` object. It applies to the whole asset: every track, every rendition, and every clip.

```json
"encryption": {
  "scheme": "cbcs",
  "keys": [
    { "tracks": "all", "key_id": "0123456789abcdef0123456789abcdef", "key": "…32 hex…", "iv": "…32 hex, optional…" }
  ],
  "systems": [
    { "system_id": "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed", "pssh": "<base64 pssh box>", "license_url": "https://license.example.net/widevine" },
    { "system_id": "94ce86fb-07ff-4f43-adb8-93d2fa968ca2", "hls_uri": "skd://asset-123" },
    { "system_id": "9a04f079-9840-4286-ab92-e65be0885f95", "pssh": "<base64 pssh box>", "license_url": "https://license.example.net/playready" },
    { "system_id": "e2719d58-a985-b3c9-781a-b030af78d30e", "license_url": "https://license.example.net/clearkey" }
  ]
}
```

| Field | Rules |
| --- | --- |
| `scheme` | `cbcs`. Anything else is rejected |
| `keys` | One entry with `"tracks": "all"` (or no `tracks`), or one `"video"` and one `"audio"` entry. Every track of the asset must end up with exactly one key. Separate video and audio keys cannot be combined with FairPlay, whose HLS key line carries no key ID: the answer is rejected with "FairPlay needs one key for all tracks" |
| `key_id`, `key` | 16 bytes each, as 32 hex digits |
| `iv` | Optional, 16 bytes as 32 hex digits: the constant IV declared in the init segment. When absent, it is the first 16 bytes of SHA-256(`"segmentor cbcs iv"` ‖ `key_id`), so every replica derives the same one. It is never derived from the key |
| `systems` | At most 8. Each names a DRM system by its standard system ID (a UUID) |
| `pssh` | Optional base64 of a complete, well-formed `pssh` box whose system ID matches, at most 16 KiB |
| `license_url` | Optional `https` URL, written into the DASH manifest (and used as Clear Key's HLS URI) |
| `hls_uri` | The HLS key URI; required for FairPlay (`skd://…`), ignored for systems HLS signals from their `pssh` |

A malformed `encryption` object makes the whole answer malformed: `502` to players, never cached as valid, like any other bad answer. The mapper client checks every rule above before the answer is used.

### Keys are secrets

Content keys are held in the same redacting type as the mapper's bearer token, so a `Debug` or log line cannot print them. They are kept in memory with the cached mapper answer and the loaded asset, and nowhere else; nothing is written to disk. They never appear in logs, error bodies, or `/admin/status`, which reports `encrypted: true` and the key IDs only. The operations guide states that the mapper answer now carries keys, so HTTPS and the bearer token (already the production requirements) protect them in transit.

### What is encrypted

Following ISO/IEC 23001-7 for `cbcs`: AES-128 in CBC mode, with the chain restarting from the constant IV at the start of each protected range and running only across the blocks that are encrypted.

- **Video (H.264, HEVC).** Each sample is a sequence of length-prefixed NAL units. Non-VCL NAL units (parameter sets, SEI, access unit delimiters) stay entirely clear. In each VCL NAL unit (slice data), the length prefix, NAL header, and slice header stay clear, and the rest is the protected range, encrypted with the 1:9 pattern: one 16-byte block encrypted, nine clear, repeating, with a trailing partial block clear. The slice header's length is found by parsing it against the sequence and picture parameter sets from the sample entry (`avcC`, `hvcC`), and from any parameter sets carried in-band earlier in the same sample. The parser is new, bounded (every read is checked against the NAL unit's length), and has a fuzz target.
- **Audio (AAC, AC-3, E-AC-3).** Each whole sample is one protected range with no pattern (every block encrypted), and a trailing partial block clear.
- **Subtitles** stay clear WebVTT.

The cipher is AES-128 from the RustCrypto `aes` crate (pure Rust, constant time, using AES-NI where the CPU has it), with the CBC chaining written by hand, because the pattern skips blocks that a library's CBC mode would chain through. It is a new dependency; `aws-lc-rs`, already in the tree, does not expose a raw block cipher suited to this.

### Boxes

- **Init segment.** The sample entry's type becomes `encv` (video) or `enca` (audio), and it gains a `sinf` box: `frma` holding the original type, `schm` with scheme `cbcs` version 1.0, and `schi` holding a version 1 `tenc` with `default_isProtected = 1`, the pattern (1:9 for video, 0:0 for audio), `default_Per_Sample_IV_Size = 0`, the key ID, and the 16-byte constant IV. `moov` gains one `pssh` box per system that supplied one. Every enclosing box's size is fixed up; the init writer already rewrites header boxes and this extends it.
- **Media fragment.** Each `traf` gains, after `trun`: `senc` (the sample count, then per sample a subsample count and its `(clear bytes, protected bytes)` pairs; no per-sample IV, because the IV is constant), `saiz` (each sample's auxiliary information size), and `saio` (one offset, from the start of the `moof`, to the first sample's entry in `senc`, matching the fragments' `default-base-is-moof` addressing). Audio samples, having no subsamples, get `saiz` sizes of zero and no `senc` entries beyond the count, as the specification allows with `default_Per_Sample_IV_Size = 0`.

### The encrypted segment path

`PreparedSegment` today is a header plus the source byte ranges to stream after it. Its body becomes one of two kinds: byte ranges (today's zero-copy path, used for every clear track) or finished bytes in memory. An encrypted track's segment is prepared by a new step:

1. Check the segment's payload against `limits.max_segment_bytes`, exactly as today, before reading anything.
2. Take a segment-job slot and read the samples' byte ranges into memory. The slot stays with the finished bytes and is handed to the response stream, which releases it when the response finishes or is aborted; no second slot is taken.
3. On the blocking pool: compute each sample's subsample map, encrypt in place, build the `moof` with `senc`, `saiz`, and `saio`, and return the finished fragment.

Everything after that is unchanged: range requests slice the finished bytes, `If-None-Match` and ETags work as before, and the response carries the same headers. A `HEAD` of an encrypted segment also reads and encrypts it, because the `senc` size depends on the sample bytes. I-frame fragments of an encrypted video track take the same path, one sample each.

Nothing encrypted is cached inside segmentor; a CDN caches it, as it caches everything else. Output depends only on the source bytes, the key, and the IV, so it is deterministic: two replicas, or two requests, produce identical bytes.

### Composites and sequences

For adaptive renditions, every rendition's tracks are encrypted with the key for their kind, and the composite's manifests carry one set of signalling. For sequences, every clip is encrypted with the same keys; the DASH signalling repeats in each Period, and the HLS `#EXT-X-KEY` lines, placed once before the first `#EXT-X-MAP`, stay in force across discontinuities because the key does not change.

### Signalling: DASH

Only for encrypted assets; unencrypted manifests are unchanged. The `MPD` declares `xmlns:cenc="urn:mpeg:cenc:2013"` and `xmlns:dashif="https://dashif.org/CPS"`. Each encrypted adaptation set gains:

- `<ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cbcs" cenc:default_KID="…"/>`, the key ID in UUID form;
- one `<ContentProtection schemeIdUri="urn:uuid:<system_id>">` per system, containing `<cenc:pssh>` when a `pssh` was supplied and `<dashif:Laurl>` when a `license_url` was.

This signals Widevine, PlayReady, Clear Key, and any other system the mapper lists.

### Signalling: HLS

One `#EXT-X-KEY` per system goes in every media playlist, before the first `#EXT-X-MAP`, and in the I-frame playlist; the master playlist repeats them as `#EXT-X-SESSION-KEY` so players can request licences before they fetch media. All use `METHOD=SAMPLE-AES`:

| System | Attributes |
| --- | --- |
| FairPlay | `URI="<hls_uri>"`, `KEYFORMAT="com.apple.streamingkeydelivery"`, `KEYFORMATVERSIONS="1"` |
| Widevine | `URI="data:text/plain;base64,<pssh>"`, `KEYID=0x<key_id>`, `KEYFORMAT="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"`, `KEYFORMATVERSIONS="1"` |
| PlayReady | `URI="data:text/plain;charset=UTF-16;base64,<PlayReady object from the pssh>"`, `KEYFORMAT="com.microsoft.playready"`, `KEYFORMATVERSIONS="1"` |
| Clear Key | `URI="<license_url>"`, `KEYFORMAT="org.w3.clearkey"`, `KEYFORMATVERSIONS="1"` |

Other system IDs have no standard HLS form and are signalled in DASH only; the mapper API reference says so. Checked in hls.js 1.7 in headless Chrome: it plays the Clear Key line, so Clear Key stays in HLS. hls.js takes the licence URL from the key line but builds its request from the init data, which Chrome's Clear Key CDM accepts only as a `pssh` with the W3C common system ID `1077efec-c0b2-4d02-ace3-3c1e52e2fb4b`; the mapper therefore lists that system with a `pssh` next to Clear Key (see the mapper API reference). A Clear Key `pssh` under the DASH-IF ID `e2719d58-…` is rejected by the CDM.

### Versioning

The URL version of an encrypted asset additionally hashes each key ID, a SHA-256 of each key, each IV, and the system list. Re-keying an asset, even under an unchanged mapper `version`, therefore gives new URLs, and a CDN never mixes segments encrypted under different keys. This needs the mapper to answer `200` with the new keys: a `304` means nothing in the answer changed, so the server keeps the keys it holds. The mapper API reference tells mapper authors to re-key with a `200` and a new `version`. `FORMAT_REVISION` does not change, because nothing served for an unencrypted asset changes.

## Security and limits

- Every field of `encryption` is validated in the mapper client before use (see [Wire format](#wire-format)); `pssh` input is size-limited and parsed as a box, never trusted as a length.
- Keys are secrets throughout (see [Keys are secrets](#keys-are-secrets)).
- Memory: each in-flight encrypted segment is held whole, under the segment-job slot taken to read it, from the read until its response finishes or is aborted; it is sent in `limits.stream_chunk_bytes` pieces, so a client that stops reading is cut off after `limits.response_idle_timeout_ms` like any other. Encrypted serving therefore holds at most `limits.max_segment_jobs × limits.max_segment_bytes`: `max_segment_jobs` defaults to min(2 × CPU cores, 32) and `max_segment_bytes` to 64 MiB, so 512 MiB on a 4-core host and up to 2 GiB. The payload buffer is allocated once at its final size, and the header is put in front of it in place. The operations guide says so and suggests lowering either limit on small hosts.
- The slice-header parser reads only within its NAL unit and fails closed: a slice it cannot parse fails that segment rather than being encrypted with a guessed clear range.
- Encryption requested for a codec this stage does not support fails the asset at load, naming the codec. Encrypted source files (`encv`, `enca`) stay rejected, as today.

## Observability

- Metrics: `vod_encrypted_segments_total`, `vod_encryption_seconds_total` (time spent reading and encrypting), and `vod_encryption_failures_total`.
- `asset_loaded` gains `encrypted` and the number of systems.
- A segment that fails to encrypt logs an error naming the asset, track, and segment, and never key material.
- `/admin/status` reports `encrypted` and key IDs per asset.

## Testing

- **Independent decryption.** FFmpeg decrypts `cbcs` with `-decryption_key`. For single files, renditions, and a sequence, the test follows the served HLS playlist and DASH manifest to each track's (and clip's) init and media segments, reassembles them, and requires that FFmpeg decodes them with no errors and the exact expected frame counts given the right key, and fails given no key or a wrong one. This validates the cipher, the subsample maps, and the boxes against an implementation we did not write.
- **Units:** the pattern cipher against published test vectors; the slice-header parser against the H.264 and HEVC fixtures (and a fuzz target in `fuzz/`); the init segment's `encv`/`enca`, `sinf`, and `pssh`, read back; each fragment's `senc`, `saiz`, and `saio`, read back, with the `saio` offset landing on the `senc` entries and the counts agreeing with `trun`.
- **Renderers:** DASH `ContentProtection` and each HLS `#EXT-X-KEY` form; unencrypted output byte-identical, checked by the conformance suite.
- **Wire and secrets:** every validation rule; keys absent from `/admin/status`, log output, and error bodies; the version changing with the key.
- **Players:** Clear Key in dash.js (and hls.js, per [Signalling: HLS](#signalling-hls)) in headless Chrome: real browser decryption without a DRM vendor. Widevine, FairPlay, and PlayReady need vendor accounts and devices; playing them is a manual release check with the operator's vendor.

## Rollout

Additive: a mapper that sends no `encryption` gets byte-identical output, and there is no configuration change. Staged as:

1. The cipher, the slice-header parser, and the boxes, tested in isolation.
2. The `encryption` wire object, the encrypted segment path, and init segments, verified by FFmpeg decryption.
3. HLS and DASH signalling, and Clear Key in real players.
4. Documentation: a mapper API "Encryption" section and the operations guide.

## Deferred

- **AV1 and VP9**, which need their own subsample rules (OBU and superframe aware).
- **Different keys per clip**, such as a clear pre-roll before an encrypted movie, which needs HLS key changes at discontinuities and per-Period DASH signalling.
- **Key rotation** within an asset.
- **Whole-segment HLS AES-128**, a separate, simpler mode that protects without DRM.
- **Encrypted source passthrough**: re-segmenting files that are already `cbcs`-encrypted.
- **A SPEKE v2 adapter**, so a mapper can hand key retrieval to a DRM vendor's standard endpoint.

## Open questions

None blocking. Clear Key in hls.js is settled at the start of implementation, as described under [Signalling: HLS](#signalling-hls).
