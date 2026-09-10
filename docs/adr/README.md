# Architectural decision records

ADRs record important architectural choices and the reasoning available when each choice was made.

## Decisions

| ID | Title | Status |
| --- | --- | --- |
| [0001](0001-use-fragmented-mp4-for-media-segments.md) | Use fragmented MP4 as the initial media segment format | Accepted |

## Status values

- **Proposed:** under discussion and not yet binding.
- **Accepted:** the current direction for implementation.
- **Deprecated:** still present but no longer recommended.
- **Superseded:** replaced by a later ADR, which must be linked.
- **Rejected:** considered and deliberately not selected.

## Workflow

1. Copy [template.md](template.md) to the next zero-padded number.
2. Describe the context and competing constraints, not only the selected technology.
3. Record meaningful alternatives and consequences.
4. Merge an ADR as `Proposed` or `Accepted`.
5. Never rewrite an accepted decision to hide history; add a superseding ADR instead.
