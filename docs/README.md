# Documentation

This book contains the project handbook, technical designs, architectural decisions, and links to the generated Rust API reference.

## Reading the book

Install the pinned mdBook release once, then start the local server with live reload:

```sh
make install-doc-tools
make book-serve
```

Build the complete static documentation site with:

```sh
make site
```

The generated book starts at `target/book/index.html`. `docs/SUMMARY.md` controls the chapters and sidebar order.

## API documentation

Application API documentation lives in `//!` and `///` comments next to Rust code and is rendered by `rustdoc`. Build only that reference with:

```sh
make doc
```

The complete `make site` output places rustdoc under the book's `/api/` path. The documentation workflow publishes the combined site to GitHub Pages after changes reach `main`.

## Technical design documents

Technical design documents describe how a feature or subsystem should work before implementation. They capture requirements, data flow, interfaces, performance constraints, risks, and validation plans.

- [Technical design index](technical-design/README.md)
- [On-demand MP4 packaging core](technical-design/0001-on-demand-mp4-packaging-core.md)
- [Technical design template](technical-design/template.md)

## Architectural decision records

Architectural decision records (ADRs) capture durable choices, their context, and their consequences. ADRs are append-only: supersede an old decision with a new ADR instead of rewriting its history.

- [ADR index](adr/README.md)
- [ADR 0001: Use fragmented MP4 as the initial media segment format](adr/0001-use-fragmented-mp4-for-media-segments.md)
- [ADR template](adr/template.md)

## Guides

- [Architecture](architecture.md) records the current boundaries and should evolve with the crate.
