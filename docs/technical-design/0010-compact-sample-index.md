# TDD 0010: Compact sample index

- Status: Accepted; implemented
- Created: 2026-10-03
- Updated: 2026-10-03
- Related ADRs: None
- Related designs: [TDD 0001](0001-on-demand-mp4-packaging-core.md) (the sample index this replaces), [TDD 0005](0005-fragmented-mp4-input.md), [TDD 0008](0008-clipping-and-concatenation.md)

## Implementation status

Implemented as designed. Every playlist, init segment, and media segment of every fixture (413 responses), and of the 60-minute benchmark asset (7,207 responses), is byte-identical to the previous build's.

| 60-minute asset, in process | Before | After |
| --- | ---: | ---: |
| Full load, minimum and median of 30 | 8.2 and 8.5 ms | 3.6 and 4.0 ms |
| Master playlist render | 1.2 ms | 0.37 ms |
| `index_bytes` | 8.9 MB | 4.4 MB |

From a fresh container, the first master playlist went from 13.6 to 6.7 ms. Master, media playlist, and first segment together went from 15.4 to 8.6 ms, against 18.9 ms for nginx-vod-module.

### What implementation found

- **Building the index costs less than expanding, but not nothing.** About 0.8 ms per track on the benchmark asset, almost all of it in the chunk table: FFmpeg wrote 213,600 chunks for 278,400 samples. When `stsc` uses every chunk once and in order, `stco` itself becomes the offset table instead of being copied.
- **The hash had been running in series with a track.** `parse_tracks` kept the first track on the calling thread and hashed after it, so the two added up. Every track now gets its own thread and the hash runs alongside them.
- **The mutation check's re-read moved off the critical path.** It now runs while the tables are parsed. For a local file it compares 256 KiB windows instead of reading a second copy of `moov`, which in a fresh process also means megabytes fewer page faults. `verify_unchanged` still runs after both, so a change that lands after the re-read is still caught.
- **The I-frame bandwidth read every sample.** It needed only the sync samples, and now walks the sync table, which took the master render from 1.2 to 0.37 ms.
- **Equivalence is tested two ways.** The fixture test compares the compact index with the old expansion on every progressive track. A randomized test compares them on 20,000 generated tables, half of them broken in one field, and requires the same samples or the same error message. Reporting the out-of-source error one step early fails that test at seed 52.
- **What is left of the first-request time is not the index.** In process, a load is about 3.6 ms. The rest of the 6.7 ms is the fresh process: starting blocking-pool and track threads, faulting in new memory, and, on the benchmark laptop's `powersave` governor, CPU frequency ramping from idle.

## Summary

A loaded track keeps its sample tables in their compact form, with one 4-byte size per sample, instead of a 32-byte record per sample. Planning and playlist rendering read the tables directly, and a segment expands only its own samples when it is requested. The first request for an asset waits on less work, and each loaded asset holds several times less memory. Every byte served is unchanged.

## Context

[TDD 0001](0001-on-demand-mp4-packaging-core.md) expands a track's tables into `Vec<Sample>`, one record per sample with its offset, size, decode time, duration, composition offset, and sync flag. On the 60-minute benchmark asset (278,400 samples), that is 8.9 MB written before the first playlist can be answered. Writing it is most of the remaining parse time: 3 to 5 ms of an 8 ms load on the benchmark laptop, bound by memory traffic rather than code ([benchmarks](../benchmarks.md#segmentor-vs-nginx-vod-module)). nginx-vod-module answers a master playlist in 4.5 ms because it reads only track headers and sums sample sizes.

The tables hold the same facts far more compactly. `stts` and `ctts` are runs, `stss` lists the few sync samples, and `stsc` with `stco` gives offsets per chunk. Only `stsz` has an entry per sample.

Every consumer of `Track::samples` does one of these things:

- reads a range of samples in order (segment and I-frame preparation, encryption);
- sums sizes over a range (bandwidth);
- looks up decode times by index, or finds the first index at or after a time (the planner, clipping, audio followers);
- walks the sync samples (the planner, I-frames, clipping);
- keeps a window of the samples and moves every decode time by a constant (edit lists, clipping, the fragmented timeline origin).

None of them needs the samples materialized all at once.

## Goals

- Serve exactly the same bytes for every asset, with the same URL versions.
- Keep only per-sample sizes, plus per-run and per-chunk tables, for each track.
- Plan segments and compute bandwidth without expanding samples.
- Keep every limit and validation that expansion performs today, with the same error messages: byte ranges inside the source, the sample-count limit, run lengths bounded before use, and decode-time overflow.

## Non-goals

- Answering the master playlist before the index is built. With this design, building the index is cheap enough that a two-stage load would add complexity for little gain.
- Persisting indexes across restarts. That remains a separate roadmap step.
- Changing the planner's rules, or any playlist or segment format.

## Design

### Storage

`media::SampleIndex` replaces `Vec<Sample>` in `Track`. It holds an `Arc` of one track's tables, shared by every window cut from it:

| Table | Contents | Size on the benchmark asset |
| --- | --- | --- |
| Sizes | One `u32` per sample | 1.1 MB |
| Chunks | First sample index (`u32`) and file offset (`u64`) of each chunk | 2.6 MB for 213,600 chunks |
| Timing runs | First sample, first decode time, and duration of each run of equal durations | 6,000 runs |
| Composition runs | First sample and offset of each run of equal composition offsets, if the track has `ctts` | 74,400 runs |
| Sync samples | Sorted, deduplicated sample indexes, or none, meaning every sample is a sync sample | 600 entries |

That totals about 4.6 MB, against 8.9 MB of records. The chunk table dominates because FFmpeg writes about one chunk per one or two samples on this file. A file with larger chunks saves more.

A timing run starts wherever durations change or decode times are not continuous. For `stts` input the runs are exactly the `stts` entries. A fragmented file's runs are built from its `trun` samples, so a gap between fragments starts a new run.

On top of the shared tables, a `SampleIndex` keeps a window (`start..end` sample indexes) and a signed decode-time shift. Edit lists, clipping, and the fragmented timeline origin each produce a new window or shift. The tables themselves are never modified or copied.

### Access

- `len`, `is_empty`, `get(i) -> Sample`: `get` binary-searches the chunk, timing, and composition tables, so it is `O(log n)`. It is meant for single lookups.
- `iter()` and `range(r)`: sequential iterators that keep a cursor into each table, so each sample costs `O(1)`. Segment preparation expands its range into a short `Vec<Sample>` with these.
- `decode_time(i)`, and `partition_by_decode_time(t)`, the first index whose decode time is at or after `t`, computed from the runs.
- `sync_indices()`, the sync samples inside the window, in order, and `nth_sync(k)`.
- `payload_bytes(r)`, the sum of sizes over a range.
- `window(r)` and `shifted(delta)`, which return new indexes sharing the tables. `shifted` checks that the first decode time stays non-negative and the last end does not overflow, so every later lookup is in range.
- `from_samples(Vec<Sample>)`, for tests and for fragmented input. Each run of contiguous samples becomes a chunk.

`Sample` stays as it is: the value type that lookups and iterators return.

### Building and validation

Parsing builds the tables in one pass over `stsc` and the chunk offsets, and one pass summing sizes per chunk. These checks keep today's messages and order:

1. Fewer or more samples mapped by `stsc` than `stsz` has, a zero chunk number, a missing chunk, and offset overflow.
2. `stts` and `ctts` sample counts, with each run bounded before use, and decode-time overflow, checked on the run totals.
3. Every chunk's last byte inside the source, which covers every sample in the chunk.

`stss` entries outside the track are ignored, as today. The table is sorted and deduplicated, because `stss` is not required to be sorted.

### Consumers

- **Planner:** walks `sync_indices()` with `decode_time`. With no sync table (an audio reference), it jumps to `partition_by_decode_time`. Followers use `partition_by_decode_time`.
- **Bandwidth:** `payload_bytes` per segment.
- **Segments, I-frames, encryption:** expand their range with `range()`, then work on the short vector as today.
- **Edit lists:** `trim_tail` scans from the end for the first sample that presents at or after the edit end. The audio priming cut uses a partition point. The shift becomes `shifted`.
- **Clipping:** windows and shifts, and the earliest presentation time over the kept window by iteration.
- **Memory accounting** (`index_bytes`): the size of the tables. Tables shared by several windows, such as clips of one file, are counted for each window, which overestimates and is safe for an eviction budget.

## Security and limits

The same limits apply before allocation. Sizes are allocated only after `stsz`'s count is checked against its box and the configured `max_samples_per_track`. Runs come from tables whose counts were checked against their boxes. Window and shift arithmetic is checked, so lookups cannot index outside the tables or produce a time that wraps. The fuzz targets run through the new code unchanged.

## Observability

No new signals. The `asset_loaded` log line and `index_bytes` report the smaller sizes.

## Testing

- **Equivalence:** for every fixture, `SampleIndex::iter()` yields exactly the samples the old expansion produced. The old expansion is kept in the tests as the reference.
- **Output:** every playlist, init segment, and media segment of every fixture is hashed before and after the change, and the hashes must be identical.
- **Unit tests** for lookups across run and chunk boundaries, partitioning at exact and in-between times, windows and shifts, and from-samples round trips.
- **Existing tests** for error messages and limits pass unchanged.
- **Benchmarks:** `make bench-compare` for cold start and load, and `index_bytes` before and after.

## Rollout

One change. Nothing is configurable, and output is identical, so there is nothing to migrate and no URL change.

## Open questions

- None. A later step could keep sizes as `u16` where every size fits, halving the largest table again.
