# ADR 0001: Use fragmented MP4 as the initial media segment format

- Status: Accepted
- Date: 2026-09-10
- Decision owners: Project maintainers
- Related designs: [TDD 0001](../technical-design/0001-on-demand-mp4-packaging-core.md)

## Context

The service must package compatible MP4 files on demand for MPEG-DASH and HLS with high throughput and without transcoding. DASH commonly carries ISO Base Media File Format fragments. HLS can carry either MPEG-2 Transport Stream or fragmented MP4.

Maintaining unrelated segment writers for the first DASH and HLS versions would duplicate timing, sample selection, buffering, and validation logic.

## Decision

Use CMAF-oriented fragmented MP4 as the initial media segment format for both DASH and HLS.

The packaging core will generate protocol-neutral initialization segments and media fragments from one segment plan. DASH MPDs and HLS playlists will be separate adapters over those shared artifacts. The initial implementation will support only a documented subset of codecs and MP4 features that can be safely transmuxed.

This decision does not claim complete CMAF conformance until automated conformance tests exist. It establishes CMAF compatibility as the design target.

## Consequences

### Positive

- One sample-index, segmentation, and fragment-writing path serves both protocols.
- Encoded sample payloads can usually be copied unchanged from the source MP4.
- HLS and DASH outputs can share timing and cache behavior.
- Fragmented MP4 provides one modern media path for both supported VOD protocols.
- Fragmented MP4 avoids implementing MPEG-TS packetization in the initial core.

### Negative

- Older HLS clients that require MPEG-TS will not be supported initially.
- Correct `moof` construction, decode timing, composition offsets, and data offsets require careful validation.
- Input codecs and sample descriptions must satisfy both the selected HLS and DASH compatibility profiles.
- Formal CMAF conformance adds constraints beyond merely producing playable fragmented MP4.

## Alternatives considered

### MPEG-TS for HLS and fragmented MP4 for DASH

This maximizes compatibility with older HLS clients but requires two media segment writers and separate timestamp/container behavior. It remains a possible later compatibility feature.

### Pre-package all assets

Pre-packaging simplifies request-time work but duplicates media in storage, delays asset availability, and conflicts with the goal of packaging arbitrary source MP4 files on demand. Generated outputs may still be cached externally.

### Delegate all packaging to FFmpeg

FFmpeg is valuable as a reference and validation tool, and may be appropriate for transcoding workflows. Running a packaging process per segment request adds process, startup, resource-control, and streaming complexity and does not provide the intended Rust-native metadata cache and request path.

### Transcode while serving

Transcoding can normalize arbitrary input and create bitrate ladders, but it changes the service into a compute-heavy encoder. It is outside the first packaging core and may later be handled by a separate preprocessing service.

## Follow-up

- Define the initial H.264/AAC input and CMAF compatibility profile.
- Validate generated fragments with FFmpeg, HLS tools, DASH tools, and representative players.
- Record a separate ADR before adding MPEG-TS output or claiming full CMAF conformance.