# Supported input

Progressive and fragmented MP4, M4A, and QuickTime `.mov` files, with `moov` first or last. Nothing is decoded or re-encoded, so the codecs must already suit the protocol:

| | Supported |
| --- | --- |
| Video | H.264, HEVC (`hvc1`/`hev1`), VP9, AV1 |
| Audio | AAC-LC, HE-AAC and HE-AACv2 (explicit signaling), AC-3, E-AC-3, Opus, FLAC |
| Layout | one video track and any number of audio tracks, or audio only; edit lists of one edit, optionally after one empty edit |
| Skipped | tracks that are not audio or video (timecode, metadata, subtitles) |
| Rejected | encrypted media, samples in `moov` mixed with fragments, external data references, more than one sample description per track, other codecs (each error names what was found) |

Whether a player can decode a codec is a separate question: HEVC and the Dolby codecs need Safari or a platform decoder, for instance. See [TDD 0004](technical-design/0004-broader-mp4-input-support.md) for what was verified where.
