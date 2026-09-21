# Security policy

segmentor reads untrusted media files and, when configured with a mapper, fetches media from
URLs that a mapper chooses. Both are places where a bug can become a vulnerability, so reports are
welcome and taken seriously.

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Use GitHub's private reporting: open the repository's **Security** tab and choose **Report a
vulnerability**. If that is not available to you, email **aminjamal10@gmail.com** with
`segmentor security` in the subject.

Include what you can of:

- the version or commit, and how it was built and configured (the `[limits]`, `[cors]`, and
  `[remote_media]` sections matter most);
- the input that triggers it: a file, a request, or a mapper answer. A small file that reproduces
  the problem is ideal;
- what happens, and what you expected.

You will get an acknowledgement, normally within a week. This is a small project, so please be
patient, and tell me if you have a disclosure deadline. I will work with you on a fix and on
crediting you if you want to be credited, and will publish an advisory when a fix is released.

## Supported versions

The project is pre-1.0. Only the latest release receives security fixes.

## What counts

These are in scope:

- a crash, hang, or unbounded memory or CPU use caused by a crafted media file, `moof` box, `sidx`,
  or mapper answer, given the configured limits;
- reading a file outside `storage.media_root`, or any path traversal;
- a mapper being able to make the server connect somewhere `[remote_media]` should forbid
  (allowed hosts, private and loopback addresses, redirects, DNS rebinding);
- a response that serves one asset's bytes under another's URL, or stale bytes under a version
  that should have changed;
- leaking a bearer token or a signed URL into logs or responses.

These are not vulnerabilities in themselves, because the documentation says so:

- the server has no TLS and no authentication and is meant to run behind a proxy or CDN that
  provides both (see [docs/operations.md](docs/operations.md));
- serving content to anyone who can reach an unprotected server;
- denial of service that needs more requests than the configured concurrency limits allow.

## Hardening already in place

`unsafe` code is forbidden by a crate-level lint. Every length read from a file is checked against
the bytes present before allocating, and arithmetic on offsets and timestamps is checked. The
parser is exercised by a deterministic corruption test on every `cargo test`, and by a fuzz target
(`make fuzz`). Remote reads are limited to configured hosts, refuse private addresses and
redirects, and are bounded in size and count.
