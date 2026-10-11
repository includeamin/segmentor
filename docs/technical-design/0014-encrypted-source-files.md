# TDD 0014: Encrypted source files

- Status: Accepted; stages 1 and 2 implemented (decrypt, one file, progressive or fragmented)
- Created: 2026-10-11
- Updated: 2026-10-11
- Related ADRs: [ADR 0001](../adr/0001-use-fragmented-mp4-for-media-segments.md)
- Related designs: [TDD 0009](0009-common-encryption-and-drm.md) (the encryption this follows and reverses), [TDD 0010](0010-compact-sample-index.md) (the index this extends), [TDD 0005](0005-fragmented-mp4-input.md) (fragmented input), [TDD 0013](0013-key-rotation.md) (the rotation that decrypted content can then use)

## Summary

The mapper can tell segmentor that a source file is already CENC-encrypted, and what to do about it:

- **Decrypt.** The answer carries the source key. Segmentor decrypts the samples in memory and serves the result clear, or encrypted again under the answer's own `encryption` or `hls_aes128`.
- **Passthrough.** The answer carries no key. Segmentor re-segments the file and keeps its original encryption, key ID and IVs, so the key never reaches segmentor.

All four common protection schemes (`cenc`, `cens`, `cbc1`, `cbcs`) are read, from progressive and fragmented files. Today an `encv` or `enca` track fails the asset at load.

## Context

[TDD 0009](0009-common-encryption-and-drm.md) protects clear sources and left "encrypted source passthrough" as deferred. Catalogs stored encrypted at rest, and catalogs delivered by a vendor already protected, cannot move to segmentor until the source can be read. nginx-vod-module repackages encrypted MP4s when given the key; this is the matching capability, plus a passthrough mode it does not have.

Two facts about the code shape the design:

- A clear segment is served from source byte ranges. A protected segment is not: its payload is read into memory and its header is built around it (`PendingEncryption` in `src/cenc/segment.rs`). Decryption fits that route.
- The sample index (TDD 0010) knows sizes and offsets and nothing about IVs. Both modes need a per-sample table beside it.

## Goals

- Decrypt `cenc`, `cens`, `cbc1` and `cbcs` sources, progressive and fragmented, on single files, adaptive assets and sequences.
- Pass an encrypted source through on single files and adaptive assets, keeping its protection exactly.
- The mapper chooses per asset. An encrypted source with no instruction keeps failing, as today.
- A wrong key is noticed at load wherever it can be (see [Decrypting](#decrypting)), instead of serving garbage that a CDN would cache.
- Output stays a pure function of the source bytes and the answer.

## Non-goals

- Deriving or fetching keys. The mapper supplies them; segmentor never talks to a licence server.
- Sources with more than one key inside a track (`seig` sample groups), or encryption of a codec segmentor cannot already encrypt (TDD 0009).
- Passthrough on sequences, for I-frame playlists, and for muxed HLS audio. Decrypted I-frame playlists work, since their samples are decrypted by the same path.
- Codecs outside TDD 0004. The original codec (`frma`) goes through the usual classification, so an encrypted track of an unsupported codec fails as a clear one does.
- Changing a passthrough asset's protection. Combining it with `encryption` or `hls_aes128` is a rejected answer.

## Design

### Wire format

The answer gains an optional `source_encryption` object. It is an opt-in: without it, an encrypted source fails at load with the message it has today.

```json
"source_encryption": { "keys": [ { "tracks": "all", "key_id": "…", "key": "…" } ] }
"source_encryption": { "passthrough": true, "systems": [ … ] }
```

- **Decrypt** has `keys`, in the shape of TDD 0009 (`tracks` is `all`, `video` or `audio`; `key_id` and `key` are 16 bytes in hex). A track is decrypted with the key whose `key_id` equals the `default_KID` of its `tenc`. A track whose key ID has no entry fails the asset, naming the key ID and never the key.
- **Passthrough** is `"passthrough": true` with no `keys`. It may carry `systems` in the shape of TDD 0009, used only for signalling (see [Passthrough](#passthrough)).
- `keys` and `passthrough` are exclusive; an object with neither is rejected.
- **Where it applies.** At the top level it covers a single file and every rendition of an adaptive asset. A clip of a sequence may carry its own `source_encryption`, which overrides the top-level one, as a clip's `encryption` does. Passthrough on `clips` is rejected.
- **Combinations.** Decrypt works with `encryption` (decrypt, then encrypt under the new key), with `hls_aes128`, and with neither. Passthrough excludes `encryption`, `hls_aes128` and muxed HLS audio.
- Keys use the redacting key type, travel in the answer's body only, and obey the existing rules for secrets in answers: https and a bearer token, no logging, no echo in errors, no appearance in `/admin/status`.
- A rejected `source_encryption` names the field and never a key.

### Reading the source

The parser stops rejecting `encv` and `enca` outright. For a protected track it reads `sinf`:

- `frma` gives the original sample entry type, which is classified as for a clear track;
- `schm` gives the scheme, one of `cenc`, `cens`, `cbc1`, `cbcs`; anything else fails the asset naming it;
- `schi/tenc` gives `default_isProtected`, the per-sample IV size, `default_KID`, the constant IV and the crypt and skip block counts.

The refusal moves to assembly: a protected track with no `source_encryption` fails with today's message. A track with `default_isProtected = 0` is clear, since mixed files exist.

For each protected sample the table holds its IV (8 or 16 bytes, or none when `tenc` has a constant IV) and its subsample list (clear bytes, protected bytes). They come from `senc`, or from `saiz` and `saio` when the file locates them that way. A progressive file keeps them under the track's sample table; a fragmented file keeps them in each `traf`, read as part of the fragment parsing of TDD 0005. When the offsets point outside `moov`, one extra read at load fetches the bytes.

The table sits beside the compact sample index, one per protected track, shared by every window cut from a clip, as the sample tables are. A one-hour 24 fps title has about 86,000 video samples, so the table is about 1 to 2 MB. It counts toward `index_bytes`.

Checked at load, each failing the asset with its own message:

- subsample sizes that do not add up to the sample's size;
- a `seig` group, or more than one key, in a track;
- a protected track with neither per-sample IVs nor a constant IV;
- `senc`, `saiz` or `saio` data that is truncated, overlapping or out of the file.

### Decrypting

A decrypt-mode asset never takes the zero-copy path. A segment's samples are read into memory as an encrypted segment's are, with the same coalesced reads. Each protected sample is decrypted with its IV and subsample list, then the fragment is finished. Decryption never changes a sample's size, so the header is built as for a clear asset. If the answer also has `encryption` or `hls_aes128`, the decrypted payload goes into that step.

The four schemes follow ISO/IEC 23001-7:

| Scheme | Cipher | Protected ranges |
| --- | --- | --- |
| `cenc` | AES-128-CTR | every byte of each protected range; the keystream runs on across a sample's subsamples; an 8-byte IV is padded with zeros to 16 |
| `cens` | AES-128-CTR | the crypt blocks of each crypt/skip pattern; the keystream runs on across encrypted blocks only |
| `cbc1` | AES-128-CBC | whole 16-byte blocks of each protected range; a trailing partial block stays clear |
| `cbcs` | AES-128-CBC | the crypt/skip pattern (typically 1:9) over each protected range, with the constant IV |

The exact chain and reset rule of each scheme is fixed by a published test vector in the implementation, and recorded under Implementation status.

**A wrong key is noticed only where it can be.** CTR and CBC turn any key into random bytes without complaint, and a CDN would keep them. The first sample of every video track is decrypted at load, and if its NAL unit length prefixes do not tile the sample exactly the asset fails, naming the track and never the key. That works only when the length prefixes are themselves encrypted, which is rare: real encoders leave each NAL unit's length and header clear and protect the rest, and decrypted slice data looks like random bytes whether or not the key was right. So for those files, and for audio and codecs without NAL units, the only check is that the key ID matches `tenc`, and a wrong key that carries the right key ID decrypts to garbage that segmentor cannot tell from video. The mapper is trusted to give the key that belongs to the key ID; changing a key gives new URLs, so a corrected key replaces cached output.

The URL version hashes a fingerprint of the source keys (SHA-256, as `encryption` does). Changing a source key gives new URLs, so a CDN never mixes output decrypted under two keys.

Throughput is lower than for clear assets because zero-copy is off. The implementation adds a decrypt benchmark to `make bench-compare`, so the cost is a measured number.

### Passthrough

The source's protection is kept, not rebuilt:

- **Init segment.** The track keeps its protected sample entry (`encv` or `enca` with `sinf`, `schm` and `tenc`), so key ID, scheme, pattern and constant IV are the source's. The source's `pssh` boxes are copied into `moov`.
- **Fragments.** The payload is the source's ciphertext. The fragment's `senc`, `saiz` and `saio` are built from the stored table for exactly the samples in the segment, using the box writers encryption already has. Cutting segments differently from the source is therefore safe. The first version uses the same in-memory route as other protected segments; because the ciphertext is never modified and the header depends only on sample sizes, serving it zero-copy is a later optimisation.
- **Signalling.** `systems` (licence URLs, `pssh` data, and for HLS a key URI) is for the manifests only. DASH always gets the `mp4protection` descriptor with the source scheme and the key ID from `tenc`, plus any `systems`. HLS is offered only for `cbcs` sources that have a system able to name an HLS key, since HLS fMP4 can carry nothing else. Any other passthrough asset is DASH-only, as an `AES-128` asset is HLS-only: its HLS endpoints answer 404.
- **Adaptive assets.** Each rendition keeps its own key ID and IVs; the shared audio group works as it does today.
- **Not offered:** I-frame playlists and muxed HLS audio, and sequences.

### Versioning

The version covers the mode, each track's scheme and key ID, and for decrypt the fingerprint of the source keys. Passthrough output depends on the source bytes alone, so its version needs nothing more.

## Security and limits

- Source keys are held in the redacting key type, in memory only. They are not logged, not in `/admin/status` and not in any error body.
- A new limit, `limits.max_source_aux_bytes` (default 64 MiB), caps the memory of a per-sample table, so a hostile file cannot make it large. Counts in `senc`, `saiz` and `saio` are checked against the sample count before anything is allocated.
- A wrong key is caught only for the rare file whose NAL unit lengths are encrypted. For any other file the mapper's key is trusted, and the test suite pins that limit so it is not mistaken for a guarantee.
- Decryption is bounded work per segment, in proportion to the segment's bytes, on the blocking pool like other segment preparation.
- A decrypted asset is as sensitive as its clear twin. Operators who decrypt should keep it behind the same authorization (TDD 0007) as any other protected title.

## Observability

`/admin/status` reports, per asset, the source mode (`decrypt` or `passthrough`), the scheme and the source key IDs. It never reports a key. The load log line names the mode. No new metrics.

## Testing

- **Decrypt, with a clear twin as the oracle.** Fixtures are encrypted by an independent test encryptor built on OpenSSL, never by the code under test, for all four schemes, progressive and fragmented, with and without subsamples and with per-sample and constant IVs. The decrypted segment must equal the clear file's segment byte for byte, and FFmpeg must decode every frame of it.
- **Decrypt then encrypt.** Decrypting a `cenc` source and serving it under `cbcs` plays back in FFmpeg with the new key; the same under whole-segment `AES-128`.
- **Wrong keys.** A missing key ID and a key for another track each fail the asset with the right message, and a wrong key fails a file whose NAL unit lengths are encrypted. No message contains a key. For a file that leaves the lengths clear, a test pins that a wrong key loads, as the limit above says.
- **Passthrough.** FFmpeg decrypts the served segments with the source key and decodes every frame; the served `senc` entries equal the source's for the same samples; segments cut differently from the source still decrypt; the DASH-only rule holds for `cenc`, `cens` and `cbc1`; `cbcs` is served over HLS only when a system can name a key.
- **Parsers.** `sinf`, `tenc`, `senc`, `saiz` and `saio` get unit tests with malformed and truncated boxes, and a fuzz target in `fuzz/`.
- **Mutations.** A skipped subsample, a crypt/skip pattern off by one, an IV off by one, `cens` treated as `cenc` and a wrong constant IV must each be caught.
- **Edges.** `HEAD`, byte ranges and ETags on decrypted segments; a clip window starting mid-file; a mixed file with a clear audio track.
- **Performance.** A decrypt benchmark in `make bench-compare`.
- **Not testable here:** real DRM devices, as for TDD 0009 and 0013.

## Rollout

Additive: an answer without `source_encryption` behaves as today. Built in four stages, each releasable:

1. Parse `sinf`, `tenc` and `senc`/`saiz`/`saio`; decrypt on a single progressive file, all four schemes, with the wrong-key check.
2. Fragmented sources.
3. Decrypt for adaptive assets and sequences, including a clip's own `source_encryption`.
4. Passthrough.

Rollback is removing `source_encryption` from the answers.

## Implementation status

Stage 1 is implemented: decrypt, on a single progressive file, for all four schemes. What differs from the design above, or settles it:

- **Aux data** is read from `senc` under `stbl` only. A file that locates its IVs with `saiz` and `saio` and has no `senc` fails with a message naming sample auxiliary information, as does a `seig` sample group. Reading outside `moov` (and the `limits.max_source_aux_bytes` limit that goes with it) arrives with the first stage that needs it: `moov` is already bounded by `limits.max_metadata_bytes`.
- **Mapper answer.** `source_encryption` takes `keys` for a single file. `passthrough`, renditions and clips are refused with a `502`, and a key entry may name `tracks` (`all`, `video`, `audio`) to restrict it to that kind of track.
- **Stage 2, fragmented sources.** A track's table is the entries of the `senc` in each of its `traf`s, in order; each `senc` must list exactly the samples of that `traf`'s `trun` boxes, and the fragments together exactly the track's samples, or the asset fails at load naming the track. A `traf` with `saiz` and no `senc`, or none and no constant IV, fails the same way, as does a `seig` group in any `traf` (even when `tenc` says unprotected). Counts are bounded by the track's own sample count before anything is allocated.
- **FFmpeg's fragmented output is not conformant for audio.** From the second fragment on, each audio `senc` lists every sample so far instead of the fragment's own (FFmpeg cannot decode that audio itself). Segmentor refuses the file rather than guess which entries are the fragment's: a wrong IV would decrypt to garbage. A test keeps the real file as the negative case; its video-only copy is the positive one.
- **Not offered for a decrypted source:** I-frame playlists and fragments, and muxed HLS audio (`packaging.hls_mux_audio` is ignored for it). Both come back when a stage needs them.
- **The wrong-key check** is the best-effort one described under [Decrypting](#decrypting).
- **`/admin/status`** lists `source_key_ids` for a decrypted asset; the mode and the scheme are not reported yet, since decrypt is the only mode.
- **Checked against independent decoders.** FFmpeg decrypts the `cenc`, `cbc1` and `cbcs` fixtures to frames bit-identical to the clear file's, and decrypts its own `cenc` output, which the tests then compare with the clear twin. `cens` has no second opinion (see Open questions).
- **Decrypt then encrypt** gives, byte for byte, the output of encrypting the clear file under the new key, and so does decrypt then whole-segment `AES-128`.
- **Metrics.** Decrypting a segment is counted with the encrypted segments, since it takes the same route.
- **Not built yet:** the decrypt benchmark in `make bench-compare` (the cost of losing zero-copy is **unmeasured**, and no document may quote a number for it), and the mode in the `asset_loaded` log line (it logs `media.encrypted`, which is about the output).
- **HEVC** is read, decrypted and announced with its real codec string, checked end to end against the clear file. `tests/fixtures/protect-fixture.py` handles `hvcC` as well as `avcC`.
- **`segmentor package`**, the development command, has no keys and refuses a protected source instead of writing it out.
- **A `seig` sample group is refused even when `tenc` says the track is unprotected** (a clear lead), and `default_isProtected` may be only 0 or 1.
- **The key check reads no sample larger than `limits.max_segment_bytes`**, which is when its segment would be refused anyway.

## Open questions

- **Telling a wrong key from a right one.** No structural check works for the usual layout. Is trusting the mapper enough, or should the mapper be able to give something to check against (for example, a hash of the first decrypted sample, or the PlayReady key checksum when the file carries a PlayReady header)?
- **The trailing partial block of a `cens` range.** Segmentor leaves it clear, as the pattern schemes do for `cbcs` (checked against FFmpeg for `cbcs`) and as Shaka Packager's pattern encryptor does. No independent decoder reads `cens` (FFmpeg does not apply its pattern), so this rests on the reference encryptor and on that reading of ISO/IEC 23001-7; confirm it against a real `cens` file before relying on it.
- **Zero-copy passthrough.** Worth building once the in-memory version has numbers?
- **A key ID shared by audio and video.** Some files protect both tracks under one key ID. A key entry with `tracks: all` covers both; confirm that is how the target catalogs' files and mappers describe them.
