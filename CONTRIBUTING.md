# Contributing

Thanks for wanting to help. This is a small project, so a short conversation before a large change
saves everyone time: open an issue describing what you want to do and why.

## Building and testing

You need a recent stable Rust toolchain and FFmpeg (only to regenerate fixtures and to run the
decode tests, which skip themselves without it).

```sh
make ci          # format check, type check, Clippy, tests, docs, fuzz-target build
make help        # every individual target
```

`make ci` must pass before a pull request is merged. The [implementation guide](docs/internals/README.md)
explains how the code is organised and [testing](docs/internals/testing.md) how it is tested.

## What a good change looks like

- **Tests first for behaviour.** A parser change comes with a fixture or a byte-mutation test, a
  protocol change with an assertion on the output, and a bug fix with a test that fails without it.
- **Bounded by construction.** Anything read from a file or a mapper is untrusted. Check a length
  against the bytes available before allocating, use checked arithmetic, and keep the limits in
  `[limits]` meaningful. `unsafe` is forbidden.
- **Lint clean.** Clippy runs with `pedantic` and warnings are errors. Prefer fixing the code to
  allowing the lint, and say why when you allow one.
- **Documented.** Behaviour that operators or mapper authors see belongs in the handbook. A change
  that alters an accepted design belongs in a design document first; see below.

## Design documents

Larger changes start as a technical design document in
[docs/technical-design/](docs/technical-design/README.md), copied from the template, and decisions
worth remembering become an [ADR](docs/adr/README.md). The existing ones show the level of detail
expected, including a section on what was found while implementing.

## Commits and pull requests

Releases are versioned automatically from [Conventional Commits](https://www.conventionalcommits.org),
so the pull request title must look like `type(scope): summary`, for example
`feat(http): add a readiness probe`. Use `!` after the type for a breaking change.

Sign off every commit to say that you have the right to submit it under the project's licence
(the [Developer Certificate of Origin](https://developercertificate.org)):

```sh
git commit -s -m "fix(mp4): reject a trun that claims more samples than it holds"
```

That adds a `Signed-off-by: Your Name <you@example.com>` line. A check on pull requests from forks looks for it.

## Licence of contributions

The project is dual licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). Unless you
state otherwise, anything you contribute is licensed the same way, without additional terms.

## No code from nginx-vod-module

[Kaltura's nginx-vod-module](https://github.com/kaltura/nginx-vod-module) is a well-known
packager and the [research notes](docs/research/nginx-vod-module.md) describe how it works from its
public documentation. **It is licensed under the AGPL-3.0**, which this project's licence is not
compatible with, so segmentor must not contain code derived from it.

Please do not copy, translate, or adapt its source, and do not implement a change while reading
its source alongside. Its documentation, its observable behaviour, and the public specifications
(ISO/IEC 14496-12, RFC 8216, ISO/IEC 23009-1) are all fine to use. The same goes for any other
project whose licence is incompatible: work from the specifications and from behaviour, not from
someone else's implementation.

## Reporting problems

- A bug or a file that will not play: use the issue templates, and for a file that will not load,
  include the error message and the output of `ffprobe -show_streams` on it.
- A security problem: do **not** open an issue; see [SECURITY.md](SECURITY.md).

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
