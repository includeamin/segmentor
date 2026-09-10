.DEFAULT_GOAL := help

CARGO ?= cargo
MDBOOK ?= mdbook

.PHONY: book book-serve book-test build check ci clean doc doc-open fixtures fmt fmt-check install-doc-tools lint serve site test

help: ## Show the available targets
	@printf '%s\n' \
		'book       Build the mdBook documentation' \
		'book-serve Build, serve, and watch the documentation' \
		'book-test  Test Rust examples in the book' \
		'build      Build the crate' \
		'check      Type-check the crate' \
		'ci         Run every CI check' \
		'clean      Remove generated artifacts' \
		'doc        Build rustdoc documentation' \
		'doc-open   Build and open rustdoc documentation' \
		'fixtures   Regenerate media test fixtures with FFmpeg' \
		'fmt        Format Rust sources' \
		'fmt-check  Verify Rust formatting' \
		'install-doc-tools Install mdBook and Mermaid support' \
		'lint       Run Clippy with warnings denied' \
		'serve      Run the example HLS service' \
		'site       Build mdBook with rustdoc under /api' \
		'test       Run unit and integration tests'

book: ## Build the mdBook documentation
	$(MDBOOK) build

book-serve: ## Build, serve, and watch the documentation
	$(MDBOOK) serve --open

book-test: ## Test Rust examples in the book
	$(MDBOOK) test

build: ## Build the crate
	$(CARGO) build --all-features --all-targets

check: ## Type-check the crate
	$(CARGO) check --all-features --all-targets

fmt: ## Format Rust sources
	$(CARGO) fmt --all

fmt-check: ## Verify Rust formatting
	$(CARGO) fmt --all -- --check

lint: ## Run Clippy with warnings denied
	$(CARGO) clippy --all-features --all-targets -- -D warnings

test: ## Run unit and integration tests
	$(CARGO) test --all-features --all-targets

doc: ## Build rustdoc documentation
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --all-features --no-deps

doc-open: ## Build and open rustdoc documentation
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --all-features --no-deps --open

fixtures: ## Regenerate media test fixtures with FFmpeg
	sh tests/fixtures/generate.sh

site: book doc ## Build mdBook with rustdoc under /api
	rm -rf target/book/api
	cp -R target/doc target/book/api

install-doc-tools: ## Install mdBook and Mermaid support
	$(CARGO) install mdbook --version 0.5.4 --locked
	$(CARGO) install mdbook-mermaid --version 0.17.1 --locked

serve: ## Run the example HLS service
	$(CARGO) run -- serve --config vod.example.toml

ci: fmt-check check lint test doc ## Run every CI check

clean: ## Remove generated artifacts
	$(CARGO) clean
