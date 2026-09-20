# TDD 0005: Fragmented MP4 input

- Status: Accepted; implemented
- Created: 2026-09-20
- Updated: 2026-09-20
- Related ADRs: None
- Related designs: [TDD 0001](0001-on-demand-mp4-packaging-core.md) (the metadata-only read strategy this changes), [TDD 0004](0004-broader-mp4-input-support.md) (phase 4, which this delivers)

## Implementation status

Implemented as designed, with the open question about a truncated final fragment still open. Several things the design did not anticipate, two of them real bugs that tests caught, are recorded below.

### What implementation found

- **The timeline origin must be found in seconds.** The first version subtracted the smallest first decode time across tracks in raw ticks. Video at 15,360 ticks per second and audio at 48,000 are not comparable, so a file starting at 100 seconds (1,536,000 video ticks, 4,800,000 audio ticks) moved its audio by the wrong amount. The offset fixture caught it. The origin is now the track whose first sample is earliest in seconds, converted into each track's ticks and rounded down so no first sample goes below zero.
- **Overlapping fragments had to be rejected, not just gaps kept.** The mutation test found a panic within seconds: a corrupted `tfdt` put a fragment before the previous one, decode times stopped being monotonic, and the planner subtracted a larger time from a smaller one (a panic in debug builds, a silent wrap to a huge duration in release). A progressive file cannot do this, since `stts` is cumulative. A fragment that starts before the previous one ended is now refused, and the planner's subtraction is checked as a second line of defence. After the fix, about 114,000 corrupted inputs across the fragmented fixtures ran with no further panic.
- **A run in another track cannot be iterated.** The legacy base-offset rule forces every `traf` in a `moof` to be measured, including other tracks'. A `trun` whose entries take no bytes can claim four billion samples, so a run that is measured but not kept computes its extent by multiplication. A test with `u32::MAX` samples in a track nobody asked for returns immediately.
- **FFmpeg's `cmaf` flag implies negative composition offsets.** That fixture's offsets are the progressive file's shifted by a constant. The equivalence test therefore compares how samples differ from the first, and compares absolute values only for variants that do not shift them.
- **FFmpeg normalises the start time to zero,** so a fixture starting at 100 seconds cannot be made with it. `generate-variants.sh` adds 100 seconds to every `tfdt` of the plain fragmented fixture instead.
- **Remote cost measured.** A three-fragment file loads from a mock origin in nine requests or fewer, and the packaged output is byte-identical to the local file's. The windowed walk is what keeps it from being one request per box header.
- **Browsers.** All six layouts play in Chrome through hls.js and dash.js; the 100-second file plays from 0.067 s, as a progressive file with B-frames does.

Not done, and stated in the design: `sidx`-driven parallel discovery for remote sources, and tolerating a truncated final `moof` in a file still being written.

## Summary

Accept MP4 files that are already fragmented (`moov` with `mvex`, then `moof` and `mdat` pairs): recorder output, CMAF files, and anything written with `-movflags frag_keyframe+empty_moov`. Today they are rejected with a message telling the operator to re-mux.

The sample index for such a file is not in `moov`, whose tables are empty. It is spread across the `moof` boxes, one per fragment. This design reads every `moof`, builds the same `MediaIndex` a progressive file produces, and leaves the rest of the pipeline untouched: planning, init segments, fragment writing, playlists, and streaming already work from that index and from byte ranges into the source.

## Context

Everything downstream of parsing consumes `MediaIndex`: per-sample offset, size, decode time, duration, composition offset, and sync flag. Media segments are produced by reading sample byte ranges out of the source, and the source's own `mdat` layout is irrelevant. A fragmented source is therefore a different way of *finding* the same facts, not a different pipeline.

Two things make it a real change and not a parser addition:

- **The read strategy.** [TDD 0001](0001-on-demand-mp4-packaging-core.md) fetches `moov` and nothing else. A fragmented file's index is in `moof` boxes scattered through the file, one per fragment, so a file with 1,800 fragments needs 1,800 small reads, and for a remote origin each is a request.
- **The timeline.** Fragmented files often start at a large decode time (recordings stamped with wall-clock time, or a segment cut from a longer stream), and their `moov` durations are zero.

## Goals

- Index fragmented files into the same `MediaIndex`, so packaging, playlists, and streaming are unchanged.
- Support the layouts real writers produce: several tracks in one `moof` or a `moof` per track, explicit and default-is-moof base offsets, all `trun` field combinations, and signed composition offsets.
- Bound the cost of discovery for both local and remote sources, and refuse files that exceed it with a message that names the limit.
- Produce output indistinguishable from packaging the same media as a progressive file.

## Non-goals

- Live or growing files. The index is built once from a file that is complete.
- Files that mix samples in `moov` with fragments. They are rejected.
- Using `sidx` or `mfra` to avoid reading every `moof`. See [Alternatives](#alternatives-considered).
- Encrypted fragments (`senc`, `saiz`, `saio`), which remain rejected at the sample entry.

## Design

### Recognising the input

A file is fragmented when `moov` contains `mvex`. Top-level `moof` boxes without `mvex` are invalid. A fragmented file with no `moof` at all has no media and is rejected. A fragmented file whose tracks also list samples in `moov` (a non-empty `stsz`) is rejected as mixed.

### Reading the fragments

`Metadata::fetch` already walks the top-level box headers to find `moov`. It now also keeps every `moof` box whole, with its offset.

The walk reads through a small window instead of 8 bytes per header. A read of 8 KiB at a box's offset usually contains the whole `moof` (a 2-second HD fragment has one to three kilobytes of `trun` entries) and the header of the `mdat` that follows it, so a fragment costs about one read, not three. A `moof` larger than the window is read again for its remainder. Everything else at the top level (`styp`, `sidx`, `emsg`, `prft`, `free`, `mfra`, `mdat`) is skipped by its header.

Limits, all checked before allocating:

| Limit | Default | Applies to |
| --- | --- | --- |
| `max_fragments` (new) | 20,000 | `moof` boxes in one file; about 11 hours at 2 s fragments |
| `max_metadata_bytes` | 64 MiB | `moov` plus every `moof`, now summed |
| `max_samples_per_track` | 2,000,000 | samples across all fragments of a track |

The existing cap of 4,096 other top-level boxes stays, and `mdat` boxes are counted with the fragments, since every fragment has one.

The cost for a remote source is roughly one request per fragment, sequentially, because each box's offset comes from the size of the one before. That is the price of this design; see the alternatives.

### Building samples

For each track, every `traf` naming it is read in file order:

- **`tfhd`** supplies the track ID, optional defaults (duration, size, flags), and the base data offset. The base is the explicit `base_data_offset` if present, else the start of the `moof` when `default-base-is-moof` is set, else the legacy rule: the start of the `moof` for the first `traf`, and the end of the previous `traf`'s data after that. The legacy rule needs the previous `traf`'s end whichever track it belongs to, so every `traf` in a `moof` is measured.
- **`tfdt`** sets the decode time of the fragment. When absent, decoding continues from the end of the previous fragment of that track.
- **`trun`** lists samples. Each field (duration, size, flags, composition offset) falls back to `tfhd`, then to the `trex` defaults in `mvex`. The optional first-sample flags override the flags of the first sample. Composition offsets are unsigned in version 0 and signed in version 1. The data offset is relative to the base; without one, a run continues where the previous run in the `traf` ended.
- **Sync** is the absence of the non-sync bit (`0x10000`) in the sample's flags.

Every sample is checked as a progressive one is: its range must end inside the source, and the running count must stay within `max_samples_per_track`. A `trun` cannot claim more entries than its bytes hold, and a `trun` whose entries take no bytes (all fields defaulted) is still bounded by that count limit before it is expanded.

A track with no samples at all is an error naming the track, for progressive and fragmented files alike.

### Normalising the timeline

Two properties of fragmented files need handling after the samples exist:

- **Start time.** The first decode time of a fragmented file is often not zero. The playlists and the DASH timeline assume a presentation that starts near zero, and a bandwidth estimate divided by an end timestamp of hours would be wrong. After edit lists are applied, the smallest first decode time across tracks is subtracted from every sample. The relative timing of tracks, and every duration, are unchanged; only the origin moves. Progressive files are not normalised, because their small positive start is deliberate (see [TDD 0004](0004-broader-mp4-input-support.md)).
- **Durations.** `mvhd`, `mdhd`, and `tkhd` durations are usually zero in a fragmented file. A track's duration is taken from its last sample instead.

Gaps or overlaps between fragments are kept as they are, because every sample carries its own decode time and the fragment writer writes it. A playlist duration absorbs a gap into the segment before it, which is a small inaccuracy and not a playback failure.

### What does not change

The init segment writer already copies from `moov`, empties the sample tables, and writes its own `mvex`, so a fragmented source's `mvex`, `mehd`, and `moof` boxes are simply not copied. The planner, fragment writer, playlists, manifest, and streaming path read the index and the source and need no change. Edit lists in a fragmented file's `moov` apply exactly as they do to a progressive one.

## Alternatives considered

- **Use `sidx` to find the `moof` boxes.** A `sidx` lists every fragment's offset and size, which would let the reads run in parallel, and CMAF files usually have one. It does not carry per-sample sizes, so every `moof` still has to be read, and files without a `sidx` need the sequential walk anyway. Parallel discovery would cut load time for remote sources with many fragments. It is a follow-up with its own trade-offs (a second code path, and trusting the `sidx` over the file), not a prerequisite.
- **Re-mux on ingest.** Tell operators to convert the file, as the error does today. Cheap for us, and it moves work and storage to every user with a fragmented source.
- **Index lazily, per fragment.** Read a `moof` only when its segment is requested. It fits the "work proportional to the request" idea, but playlists need every segment's duration up front, so the index must be complete before the first playlist is served.

## Security and limits

The trust boundary is unchanged. Every count read from a `moof` is checked against the bytes present before allocation, offsets use checked arithmetic (a data offset is signed), and the limits above bound the number of fragments, the metadata bytes, and the samples. The fragment walk is bounded by the source length and the box count. A hostile `trun` cannot allocate: entries take at least the bytes they declare, and entry-less runs are bounded by the sample limit before expansion.

## Observability

- The `asset_loaded` event gains `media.fragments` for fragmented sources.
- Load failures for exceeding a fragment or metadata limit name that limit.

## Testing

- **Equivalence with a progressive file.** Fragmented fixtures are made by remuxing the existing progressive fixture with `-c copy`, so they hold the same packets. A test parses both and compares every sample: size, duration, composition offset, sync flag, and the payload bytes at each offset. Decode times must match too once the origin is normalised.
- **Layouts.** One fixture per writer behaviour: both tracks in one `moof`, a `moof` per track (CMAF), no `default-base-is-moof` (explicit base offsets), a `sidx` present, and a start time of 100 seconds.
- **Unit tests** for `trun` field combinations and defaults, first-sample flags, version 1 negative offsets, legacy base offsets across several `traf`s, missing `tfdt`, and every rejection (mixed samples, `moof` without `mvex`, no fragments, over the fragment limit, truncated boxes, a run that claims more entries than it holds).
- **The existing suites.** The conformance suite gets the fragmented fixtures, so every fragmented source is audited as HLS and DASH, decoded by FFmpeg, and compared with its source.
- **Mutation.** The corrupted-metadata test also corrupts `moof` bytes.
- **Browsers.** Fragmented fixtures play in Chrome through hls.js and dash.js.

## Rollout

One change. Fragmented files that used to fail now load; nothing that worked changes, apart from the error for a track with no samples. `max_fragments` is a new key in `[limits]`, documented in [operations](../operations.md).

## Open questions

- **Remote sources with many fragments.** Sequential discovery costs one round trip per fragment. Is 20,000 the right default cap for remote origins, or should remote sources have a lower one until discovery is parallel?
- **Growing files.** A file still being written has a `moov` and a partial last fragment. Should a truncated final `moof` be dropped instead of failing the file?
