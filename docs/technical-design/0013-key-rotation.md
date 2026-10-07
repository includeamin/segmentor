# TDD 0013: Key rotation within an asset

- Status: Accepted; implemented for single-file assets
- Created: 2026-10-07
- Updated: 2026-10-07
- Related ADRs: [ADR 0001](../adr/0001-use-fragmented-mp4-for-media-segments.md)
- Related designs: [TDD 0009](0009-common-encryption-and-drm.md) (the `cbcs` encryption this extends), [TDD 0012](0012-hls-aes-128.md) (rotation there is a separate, later step)

## Summary

The mapper can give an asset a list of **key periods**: consecutive stretches of the timeline, each with its own content key, DRM signalling, or no encryption at all. A period starts at a segment boundary. Segmentor encrypts each segment under its period's key, tells the player which key a segment uses, and keeps one initialization segment for the whole asset. The first period can be clear, which gives the "clear lead" that nginx-vod-module offers as `vod_drm_clear_lead_segment_count`.

## Context

[TDD 0009](0009-common-encryption-and-drm.md) gives an asset one key for the whole timeline (or one for video and one for audio). Studios and DRM vendors often require more:

- **Rotation.** A new key every few minutes bounds what one leaked key exposes, and is a licence condition for some high-value content (live and premium VOD alike).
- **Clear lead.** The first seconds play without waiting for a licence, which cuts start-up time.

TDD 0009 already supports different keys per clip, and a clip sequence can express rotation by cutting one file into windows. That produces a discontinuity (HLS `EXT-X-DISCONTINUITY`, DASH Periods) at every key change and a different init segment per clip, which players handle but not always seamlessly. This design makes rotation a property of the asset, with no discontinuity.

## Goals

- Key periods for single-file assets, adaptive assets, and (where all clips agree) sequences, for HLS and DASH.
- One init segment per track for the whole asset, so the player's pipeline is not reset at a key change.
- A clear period at the start, and clear periods anywhere else, as a special case of a period.
- Output that is deterministic and cacheable like everything else: a segment's bytes depend only on its samples and its period.

## Non-goals

- Deriving keys. The mapper supplies every key, as it does today; segmentor never contacts a licence server.
- Rotation for `AES-128` whole-segment encryption ([TDD 0012](0012-hls-aes-128.md)).
- Rotation on a time schedule that segmentor computes. The mapper lists the periods.
- Changing keys inside a segment.

## Design

### Wire format

`encryption` gains an optional `periods` list in place of a single set of `keys`/`systems`:

```json
"encryption": {
  "scheme": "cbcs",
  "periods": [
    { "start_ms": 0,      "clear": true },
    { "start_ms": 12000,  "keys": [ { "tracks": "all", "key_id": "…", "key": "…" } ], "systems": [ … ] },
    { "start_ms": 600000, "keys": [ { "tracks": "all", "key_id": "…", "key": "…" } ], "systems": [ … ] }
  ]
}
```

- `keys` and `systems` inside a period follow the rules of [TDD 0009](0009-common-encryption-and-drm.md#wire-format). A period has either `keys` or `"clear": true`.
- `periods` and the top-level `keys` are exclusive. An answer with only top-level `keys` is a single period starting at 0, exactly as today.
- The first period starts at 0, `start_ms` strictly increases, there are at most 256 periods, and no two adjacent encrypted periods share a key ID (that would be a mapper mistake, not a rotation).
- A period begins at the first segment whose start time is **at or after** `start_ms`; the mapper API reference says so, and `/admin/status` reports the segment index each period actually starts at.
- Every constraint of 0009 holds for every period (for example, FairPlay cannot be combined with separate video and audio keys).
- The reason a bad `periods` list is rejected names the field and never a key.

### The init segment

Every encrypted track's init segment is the one from TDD 0009, with `tenc` describing the **first encrypted period's** key ID and IV. When the asset starts clear, `tenc` still declares `default_isProtected = 1`, so a player prepares its decryption pipeline for the track once.

A fragment whose key differs from `tenc`, or that is clear, overrides it with sample groups (ISO/IEC 23001-7, section 6):

- `sgpd` of type `seig` with one entry: `isProtected`, per-sample IV size, the key ID, and the constant IV (for `cbcs`);
- `sbgp` of type `seig` mapping every sample of the fragment to entry 1.

Both go in the fragment's `traf`, next to `senc`. A fragment under the `tenc` key carries neither, so an unrotated asset is byte-identical to today's output.

A clear fragment uses `isProtected = 0` in the `seig` entry and carries no `senc`, `saiz`, or `saio`.

### Signalling

**HLS.** In every media playlist (and the I-frame playlist), the `EXT-X-KEY` lines of a period come immediately before the first segment of that period. A clear period is `EXT-X-KEY:METHOD=NONE`. There is no `EXT-X-DISCONTINUITY`, and `EXT-X-MAP` appears once. The master playlist's `EXT-X-SESSION-KEY` lines are the union over all periods, deduplicated by RFC 8216 identity, so a player can request licences ahead of time.

**DASH.** The `AdaptationSet`'s `ContentProtection` describes the first encrypted period (`default_KID`, the systems' `pssh` and licence URLs). Each fragment's `moof` carries the `pssh` boxes of its own period, which is how EME-based players learn of later keys (ISO/IEC 23001-7, section 8.1; this is how production packagers rotate keys). The manifest is otherwise unchanged, so there is still one Period.

### The segment path

TDD 0009's encrypted segment step takes the period for the segment's index: `index_of_period(segment) -> &Period`. The key, the `seig` group, and the in-fragment `pssh` follow from it. A clear segment skips encryption and is served from the source ranges (the zero-copy path), plus the rewritten `moof` that declares the clear `seig` group.

### Versioning

The URL version hashes every period's start, key ID, key (as its SHA-256), IV, and system list. Re-keying any period gives new URLs for the whole asset, as in TDD 0009: a CDN never mixes keys under one version.

## Security and limits

- Keys are held in the same redacting type as today, in memory only, and are never logged, never in `/admin/status` (which reports key IDs and period boundaries), and never in error bodies.
- The mapper answer's size limit bounds the period list. The 256-period cap bounds per-asset key memory and the `EXT-X-KEY` lines in a playlist.
- Rotation does not weaken the property of TDD 0009 that segment bytes are a pure function of the source, the keys, and the IVs.

## Observability

`/admin/status` reports, per asset, `key_periods: [{ start_segment, key_id | clear }]`. No new metrics.

## Testing

- The wire format: valid lists, and every rejection (unsorted, duplicate starts, first period not at 0, both `keys` and `periods`, a period with neither `keys` nor `clear`, adjacent identical key IDs, too many periods) without echoing a key.
- Boundaries: a period starting mid-segment begins at the next segment; periods that map to the same segment are rejected.
- Init segments: `tenc` carries the first encrypted period's key; an unrotated asset's init and fragments are byte-identical to today's.
- Fragments: `seig` `sgpd` and `sbgp` appear exactly when the period's key differs from `tenc`, a clear period has `isProtected = 0` and no `senc`.
- Decryption with an independent oracle: for each period, init plus that period's segments are decrypted with FFmpeg under that period's key and every frame is decoded; a segment decrypted with another period's key does not decode. FFmpeg's trace output confirms the `seig` entries.
- HLS playlists: `EXT-X-KEY` placement before the first segment of each period, `METHOD=NONE` for clear periods, no discontinuity, one `EXT-X-MAP`, `EXT-X-SESSION-KEY` union.
- DASH: `pssh` per fragment and a single Period.
- `HEAD`, byte ranges, and ETags across a period boundary.
- Mutations: `seig` omitted, key off by one period, `METHOD=NONE` missing, boundary rounded down instead of up. Each must be caught.
- Not testable here: real devices. Like TDD 0009, a release check with an operator's DRM vendor account covers Widevine, PlayReady, and FairPlay. hls.js and dash.js are checked with Clear Key.

## Rollout

Additive. An answer with `keys` and no `periods` is unchanged. Staged:

1. Wire format, validation, and the fragment boxes, for a single-file asset, HLS and DASH.
2. Adaptive assets (the period list applies to every rendition at the same segment indexes).
3. Sequences whose clips agree on one period list; other mixes keep using per-clip keys.

## Implementation status

Stage 1 (single-file assets, HLS and DASH) is implemented. What differs from, or settles, the design above:

- **`clear_lead_ms`** exists, as sugar for a clear first period. It was an open question and the answer was yes.
- **Boundaries** are segment-aligned, rounded up. Two periods that land on one segment make the asset fail to load (`500`, like any load failure), and a period past the last segment is dropped.
- **Every encrypted fragment** of a rotating asset carries its period's `pssh` boxes, including the first period's, not only later ones. A `seig` group appears only where the key differs from `tenc`.
- **Clear periods** go through the encrypted-segment path (read into memory, header rebuilt), because their fragments must say they are clear. They are not zero-copy.
- **Not offered:** I-frame playlists for assets with periods; periods on adaptive assets and sequences (the answer is rejected). Both remain Stage 2 and 3.
- **`/admin/status`** reports every period's key IDs, not their segment boundaries.
- **Checked with FFmpeg.** FFmpeg takes a fragment's key and IV from `tenc` and ignores a `seig` group's IV, so the test judges each later period against an init segment whose `tenc` names that period's key and IV, and checks the `seig` entry byte for byte separately. FFmpeg decodes a clear fragment without a key. Real-device playback of `seig`-signalled rotation is not verified here.

## Open questions

- **Players that ignore `seig`.** Some older MSE players read only `tenc`. Is `seig` plus in-fragment `pssh` enough for the target players, or do we also need an init segment per period as an option (at the cost of a discontinuity)?
- **Segment-aligned periods only.** Is rounding up to the next segment boundary acceptable to the licence rules this is meant for, or do operators need to pin the segment duration so that a boundary falls exactly where they want it?
- ~~Clear lead as its own field~~: added as `clear_lead_ms`.
