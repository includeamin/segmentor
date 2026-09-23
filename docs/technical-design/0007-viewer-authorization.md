# TDD 0007: Viewer authorization

- Status: Draft
- Created: 2026-09-23
- Updated: 2026-09-23
- Related ADRs: None
- Related designs: [TDD 0002](0002-asset-map-interface.md) (the mapper interface this sits beside, not inside), [TDD 0006](0006-trick-play-subtitles-and-renditions.md) (renditions this can scope, and DRM, which stays deferred)

## Summary

Two additive mechanisms, chosen so the common case costs no extra request:

1. **Signed playback tokens** (the default, and the only mechanism most deployments need): the operator's own system decides who may watch what and mints a short-lived, scoped token. segmentor verifies it locally, on every request, with no network call of its own.
2. **A live authorization check** (opt-in, per deployment): for the smaller set of cases that need real-time revocation — banning a viewer mid-stream, capping concurrent sessions — that a pre-issued token cannot express, segmentor asks an external HTTP authorizer, cached briefly, the same shape as a mapper resolution.

Neither changes behavior when `[authorization]` is absent from the configuration: the origin serves exactly as it does today. That is why the existing "authorize at the reverse proxy or CDN" guidance in [deployment](../deployment.md#before-you-expose-it) remains correct and unaffected — this design is a second option for when the decision belongs in segmentor itself, not a replacement for the first.

## Context

segmentor has no authentication today, by design: [deployment](../deployment.md#before-you-expose-it) tells operators to authorize at a reverse proxy or CDN, in front of an origin that trusts everything reaching it. That is enough when the proxy already has what it needs to decide — a signed CDN URL, a fixed source IP, mTLS. It stops being enough when the decision needs to know about the *asset* (which of the renditions or subtitle languages TDD 0006 lets a mapper list this viewer may see, not just "is this request allowed at the edge at all"), or when an operator would rather make the decision once, in the one system that already knows the catalog and the URL structure, than duplicate it across every proxy and CDN configuration in front of it.

Two facts drive the design, and they pull in the same direction:

- **A decision made once must not become a per-request problem.** One HLS or DASH session is a manifest, then an init segment and many media segments per track, repeated for as long as playback continues. If every one of those called out over the network to an authorizer, that would be most of segmentor's request volume turned into synchronous external latency on the hot path — a completely different cost profile from the mapper, which resolves an asset once and shares that answer across every viewer.
- **The registry's cache is already shared across viewers, on purpose, and must stay that way.** `AssetRegistry`'s resolution and loaded-asset caches are keyed by `(asset_id, version)` (see `registry/mod.rs`), not by viewer, and sharing them is exactly what keeps a popular asset cheap to serve. A viewer-scoped answer cannot be folded into that cache without either fragmenting it per viewer — which throws away the sharing — or living somewhere else entirely. This design keeps authorization completely separate from asset resolution and caching.

## Goals

- A request without a valid grant gets refused on every URL a title exposes — playlists, manifests, init segments, media segments, I-frame and subtitle resources — not only the master playlist, since a client that has once seen a playlist can otherwise construct the rest.
- The common case (a token already granted) adds no network call and no measurable latency.
- An operator can express "this grant covers renditions A and B but not C" and "this grant covers this asset until this time," without segmentor knowing anything about accounts, subscriptions, or billing.
- A deployment that genuinely needs live revocation can opt into it, at a cost it explicitly accepts, without changing the token format everyone else uses.
- Zero behavior change, zero cost, for a deployment that does not configure `[authorization]`.

## Non-goals

- **DRM.** Encryption, license servers, and key rotation solve "a viewer who has the bytes cannot redistribute them," which is a different, larger problem than "may this request have the bytes at all." TDD 0006 already keeps that separate and deferred; this design does not revisit it, and does not require it.
- **Issuing tokens.** segmentor verifies them. Minting them — and everything upstream of that: login, entitlement, payment — is the operator's own system, the same way the operator's system already decides what to tell the mapper.
- **Viewer identity or sessions.** A token is an opaque, scoped grant. segmentor never learns who a viewer is, only what a token claims.
- **Per-segment external calls.** The live-check mode (below) is scoped to a cacheable decision, not to every request; see Goals.
- **Changing what the mapper protocol carries.** Authorization and asset resolution are independent trust boundaries (see Security and limits); this design adds no fields to the mapper's answer.

## Design

### Where the check happens

A router-level check scoped to the HLS and DASH route groups only — `/hls/*` and `/dash/*` — not `/health`, `/ready`, `/metrics`, or `/admin/status`, which must stay reachable for the process and its operators regardless of viewer authorization. It runs early, right after the existing header-size and load-shedding checks and before the route handler, so a refusal costs no asset resolution and no packaging work.

Disabled by default: with `[authorization]` absent, the check is not added to the router at all, not merely short-circuited, so a deployment that never configures it pays nothing — no branch, no allocation, no clock read.

### Mode 1: signed tokens (the default)

**Format.** JWT, restricted tightly rather than accepted broadly: exactly one algorithm is configured (`HS256` with a shared secret by default, or `ES256`/`EdDSA` with a public key for deployments where the issuer is a separate, less-trusted system), a token's header must name exactly that algorithm, and anything else — including `alg: none` — is refused outright, with no negotiation and no fallback. JWT rather than a bespoke format because operators wiring this into an existing entitlement system almost always already have something that issues JWTs (an IdP, an API gateway, a custom backend); a bespoke format would just mean they write a translator in front of it. The restriction to one fixed, explicit algorithm closes the usual JWT footguns (algorithm confusion, `none`) by construction rather than by relying on a general-purpose library's defaults.

**Claims segmentor understands:**

| Claim | Required | Meaning |
| --- | --- | --- |
| `exp` | Yes | Standard expiry. Enforced with a small, configurable clock-skew tolerance. |
| `nbf` | No | Standard not-before. |
| `asset` | Yes | The asset ID this grant covers, or a short explicit list. No wildcards or patterns — a grant names what it covers, the same "explicit, not inferred" posture path and host validation already take elsewhere in this codebase. |
| `renditions` | No | Rendition IDs (TDD 0006) this grant covers; absent means all of them. |
| `subtitles` | No | Language tags this grant covers; absent means all of them. |

Unknown claims are ignored, so the format can grow — the same forward-compatibility rule the mapper's wire `Wire` structs already apply with `serde`'s field-level defaults.

**Transport.** Two supported, an operator's choice:

- **Query parameter** (`?auth=...`, alongside the existing `?v=`). segmentor's own HLS and DASH renderers append it to every URL they emit, exactly as they already append `?v=` — so a player that only ever follows the URLs a playlist gives it carries the grant forward automatically, with no player-side configuration. This is the only transport that works uniformly everywhere, including native players (Safari's built-in HLS, most mobile and TV players) that a page cannot make attach a custom header.
- **Cookie**, scoped to the asset's path. Needed because a query-string token is part of the URL, and therefore part of a CDN's cache key: it turns a shared, immutable segment into an effectively per-viewer one, which is the entire tradeoff a CDN exists to avoid (see Caching, below). A cookie authorizes the same way without appearing in the URL, so the cache key — and the sharing — survives. The cost is that not every player has a cookie jar, and cross-origin delivery needs the same care `[cors]` already documents for the demo and admin pages.

**Ordering and the "no oracle" property.** The check runs before the asset is resolved. A request refused for a bad token therefore never causes a resolver lookup, which means an unauthorized caller learns nothing about whether the named asset exists — the ordering itself prevents that leak, without needing special-cased error messages.

**Failure codes.** Missing or unparsable token: `401` (a credential problem — log in again). Expired or not-yet-valid: `401`, with a reason in the body (`token expired`) but nothing about the asset. Well-formed token that does not cover the requested asset (or rendition, or subtitle language): `403`. None of these bodies ever confirm or deny that the asset itself exists.

### Mode 2: a live check (opt-in, composes with tokens)

For the smaller set of deployments that need revocation faster than a short `exp` can provide — banning a viewer while they are watching, enforcing a concurrent-stream cap — a second, optional gate: segmentor asks an external HTTP authorizer, and caches the answer briefly. Structurally this reuses the mapper's `HttpResolver` machinery (connect/request timeouts, retries, a positive TTL, a shorter negative TTL, `stale_if_error`) rather than reinventing it, because the cost/caching shape is the same problem the mapper already solved: `GET {base_url}/v1/authorize?asset={asset_id}&grant={opaque hash of the token}` → `{"allowed": true, "ttl_seconds": N}`, cached per `(asset_id, grant hash)` so a session's later requests do not re-ask until the TTL expires.

This is additive to Mode 1, not a replacement: a token still establishes what a grant covers; the live check adds "and is this grant still good right now," checked far less often than every request because of the cache, the same amortization the mapper already relies on.

**Failure behavior when the authorizer itself is unreachable** is a real operator choice, exposed as `fail_open` (serve using the last-known-good decision within a grace window, mirroring `stale_if_error`) or `fail_closed` (refuse until it recovers). Default `fail_closed` — failing open on a resolver outage just means stale metadata; failing open on an authorization check means anyone gets in, a materially worse default.

### Caching

Per-viewer authorization and shared-edge caching pull in opposite directions, and this design does not pretend otherwise:

- A **query-parameter token** is part of the URL, so it is part of any CDN's cache key by default. Fine-grained (near-per-viewer) tokens mean near-zero cache sharing on segment bytes — every viewer effectively gets their own cached copy, which is worse than today's immutable, `?v=`-keyed URLs that every viewer already shares. This is not a segmentor-specific problem; it is the same tradeoff behind CloudFront and Akamai's signed URLs, which have exactly this cost.
- A **cookie token** keeps the cache key URL-only, so segment sharing survives, provided the CDN is told to vary its cache on the URL alone and forward (not cache-key) the cookie to the origin for the authorization check itself. Most CDNs support this split; it needs to be configured deliberately, and is worth a worked example in operations documentation once a transport ships (see Open questions).
- **Grant granularity is an operator knob, independent of the mechanism.** A token scoped to one content tier or one geographic region, shared by every viewer entitled to it, recovers most of the caching benefit at the cost of coarser revocation (revoking the tier revokes everyone on it); a token scoped to one viewer is the opposite trade. Nothing in this design forces either choice.

## Security and limits

- Secrets and keys are `Secret`-wrapped, never logged or `Debug`-printed, and read from an environment variable named by config — never the value itself in the configuration file — the same posture the mapper's bearer token already has.
- Exactly one configured algorithm is accepted, checked exactly; `alg: none` and algorithm negotiation are refused outright, not merely discouraged.
- Clock-skew tolerance is bounded and configurable, not unlimited.
- A live-check request carries only an opaque grant hash and the asset ID, never anything else about the request or the viewer, so the authorizer cannot be used to learn more than its own token already encoded.
- Authorization and asset resolution are independent trust boundaries: a compromised mapper cannot forge a viewer's grant (it never sees or issues tokens), and a compromised or leaked token cannot make segmentor fetch from an origin the `[remote_media]` policy would not already allow (verification and resolution are separate code paths that never share state).
- New bounded inputs, checked before anything is parsed: a maximum token size, and (Mode 2) a maximum authorizer response size, mirroring the mapper's `max_response_bytes`.

## Observability

- `vod_authorization_checks_total{outcome="granted|denied|expired|malformed"}`.
- Mode 2 adds `vod_authorization_live_requests_total{outcome=...}`, mirroring `vod_resolver_requests_total`.
- `/ready` can optionally reflect the live authorizer's reachability, behind its own probe interval, the same way it already reflects the mapper's.
- A denial logs at `debug` — normal, high-volume operational noise, the same level ordinary media requests already use — naming the reason but never the token's contents.

## Testing

- Token verification, one case per named reason: valid, expired, not-yet-valid, wrong asset, wrong rendition, wrong algorithm, tampered signature, malformed, oversized.
- Every route family — HLS and DASH master, media playlist, init, segment, I-frame, and subtitle resources — refuses consistently without a valid grant, as an exhaustive test over `ROUTES` (mirroring `route_templates_are_unique`'s discipline), so a change to one route cannot silently reopen another.
- Both transports round-tripped through a real HTTP client, not only unit-level claim parsing.
- Mode 2: granted, denied, timeout, malformed response, and the `fail_open`/`fail_closed` switch, following the existing `MockMapper` test harness's shape.
- A conformance pass with authorization configured, confirming hls.js and dash.js still play end to end when the demo and admin pages are given a valid token.

## Rollout

Additive and disabled by default: with `[authorization]` absent, the router is unchanged and every existing deployment — including this repository's own docs and examples — is unaffected. Signed tokens (Mode 1) ship first and are the complete feature for most deployments; the live check (Mode 2) is a second, independent stage that composes with it and can land later without revisiting the token format. `docs/mapper-api.md` and `examples/mapper/README.md` gain a short note once Mode 1 ships, since the example mapper and the demo/admin pages built alongside it are the easiest way to see it work.

## Open questions

- **Token minting.** Does segmentor ship a small CLI (`segmentor sign-token ...`) for operators without their own JWT tooling, or is issuing anything, even for convenience, scope creep for a project that otherwise never mints credentials?
- **Cookie transport and CORS.** The demo and admin pages already need `[cors]`; a cookie-based grant adds `SameSite` and credentialed-fetch questions those pages, and any browser-based player, would need to handle. Worth resolving before or after the query-parameter transport ships?
- **Revocation without Mode 2.** A short `exp` is the only bound on a compromised token's lifetime when the live check is not configured. Is that enough for a default, or does even the token-only mode need a cheap denylist?
- **A worked CDN example.** Query-parameter tokens fragment the cache per grant; cookie tokens need the CDN configured to keep the cache key URL-only. Should [operations](../operations.md) gain a concrete example (a CloudFront function or a Cloudflare Worker) once a transport is chosen, the way [deployment](../deployment.md) already has one for Compose and Kubernetes?
- **Per-request grant hash cost for Mode 2.** Hashing the token on every request to key the live-check cache is cheap, but worth confirming against the same latency budgets `benches/budgets.rs` already enforces, rather than assuming.
