# Technical design documents

Technical design documents explain a proposed subsystem before or during implementation. They may evolve as experiments reveal new constraints.

## Documents

| ID | Title | Status |
| --- | --- | --- |
| [0001](0001-on-demand-mp4-packaging-core.md) | On-demand MP4 packaging core | Accepted; verification pending |
| [0002](0002-asset-map-interface.md) | Asset mapper interface | Accepted; implemented |
| [0003](0003-production-grade-http-api.md) | Production-grade HTTP API | Accepted; implemented |
| [0004](0004-broader-mp4-input-support.md) | Broader MP4 input support | Accepted; implemented |
| [0005](0005-fragmented-mp4-input.md) | Fragmented MP4 input | Accepted; implemented |
| [0006](0006-trick-play-subtitles-and-renditions.md) | Trick play, subtitles, and adaptive renditions | Draft |

## Workflow

1. Copy [template.md](template.md) to the next zero-padded number.
2. Keep the document in `Draft` while major questions remain.
3. Record architectural choices as ADRs and link them from the design.
4. Change the status to `Accepted` before implementation becomes the reference behavior.
5. Use `Superseded` when a replacement design is accepted, and link both documents.
