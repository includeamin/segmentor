# Releasing

Versions are tagged automatically after every merge, and a release is published by hand when you want one. Both follow [Semantic Versioning](https://semver.org) and are driven by [Conventional Commits](https://www.conventionalcommits.org).

## The commit convention

Every commit that lands on `main` needs a subject of the form `type(scope): summary`. With **squash merges** (recommended) the pull request title becomes that subject, and the `PR title` workflow rejects titles that do not follow it.

| Type | Effect on the version |
| --- | --- |
| `feat` | Minor bump (`1.2.0` to `1.3.0`) |
| `fix`, `perf` | Patch bump (`1.2.0` to `1.2.1`) |
| Any type with `!` (`refactor(api)!: ...`), or a `BREAKING CHANGE:` line in the body | Major bump (`1.2.0` to `2.0.0`) |
| `docs`, `refactor`, `test`, `build`, `ci`, `chore`, `style`, `revert` | No release by themselves |

The highest level in a merge wins. While the version is `0.x`, a breaking change bumps the **minor** number instead, so `0.4.0` becomes `0.5.0`; the first breaking change after `1.0.0` is a major bump. Commits that do not follow the convention never trigger a release but still appear in the changelog under "Other changes".

Example, for a change that needs a note:

```text
feat(resolver)!: require a version in mapper answers

BREAKING CHANGE: answers without a `version` are now rejected.
```

## Automatic tags: the `Tag release` workflow

After the **CI** workflow succeeds on `main`, `.github/workflows/tag.yml` runs `.github/scripts/next-version.sh` for the tested commit:

1. It finds the newest `vX.Y.Z` tag reachable from that commit. Pre-release tags such as `v1.0.0-rc.1` are ignored.
2. It reads the non-merge commits since that tag and picks the highest level from the table above.
3. If there is a releasable change it creates an annotated tag `vX.Y.Z` on the tested commit and pushes it. Otherwise it records why nothing was tagged in the run summary.

The very first tag uses the version in `Cargo.toml` (currently `0.1.0`), so the history starts from a known point. After that `Cargo.toml` is **not** edited on `main`; the tag is the source of truth, and the release build stamps the tag's version into the binary (see below). This avoids a commit-back step that branch protection would block.

Tagging is sequential (one run at a time) and idempotent: re-running a workflow never creates a second tag for the same commit or version. It creates a tag only, not a GitHub release, so it does not need the release assets.

To preview what the next tag would be, run the script locally:

```sh
.github/scripts/next-version.sh
```

## Publishing a release: the `Release` workflow

Run **Actions → Release → Run workflow** and choose:

| Input | Meaning |
| --- | --- |
| `tag` | The version tag to release, for example `v1.3.0`. Leave it empty for the newest tag |
| `channel` | `latest` marks the release as the latest one. `preview` marks it as a pre-release, and it will not become "latest" |

The workflow then runs these jobs:

1. **Prepare.** Builds the changelog for that tag from the commits since the previous version tag, grouped into Breaking changes, Features, Bug fixes, Performance, Documentation, Refactoring, Build and CI, Tests, and Other changes, with links to each commit and to the full comparison.
2. **Build**, once for `x86_64-unknown-linux-gnu` and once for `aarch64-unknown-linux-gnu`, each on a native runner. It checks out the tag, stamps its version into `Cargo.toml` and `Cargo.lock` for the build only, builds `vodnik` in release mode with `--locked`, and packages `vodnik-vX.Y.Z-<target>.tar.gz` (the binary, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, and `vod.example.toml`) with a SHA-256 file.
3. **Publish.** Attests the build provenance of each archive, then creates the GitHub release, or **updates it if it already exists** (notes, title, and channel), and uploads the assets.
4. **Container.** Runs the [`Container image`](../.github/workflows/container.yml) workflow: builds the image natively for `linux/amd64` and `linux/arm64`, publishes one multi-architecture image to `ghcr.io/<owner>/vodnik`, signs it with cosign, and attests its provenance. It is tagged `X.Y.Z` and `X.Y`, plus `latest` or `preview` according to the channel. That workflow can also be run by hand for an existing tag.

Running the release again for the same tag is safe, so a preview can be promoted to latest by re-running with `channel = latest`. A preview release is titled `vX.Y.Z (preview)`.

## Publishing the crate: the `Publish crate` workflow

Run **Actions → Publish crate → Run workflow** to publish the crate for a version tag to [crates.io](https://crates.io). Publishing cannot be undone, only yanked, so the workflow is manual and has two safeguards.

| Input | Meaning |
| --- | --- |
| `tag` | The version tag to publish, for example `v1.3.0`. Leave it empty for the newest tag |
| `dry_run` | On by default. Packages the crate and builds it from the package, which proves the `exclude` list in `Cargo.toml` kept everything the code needs. Turn it off to publish for real |

The workflow runs two jobs:

1. **Package and check.** Resolves the tag (it must be an existing `vX.Y.Z` tag on the default branch), asks crates.io whether that version is already published, stamps the tag's version into `Cargo.toml` and `Cargo.lock`, and runs `cargo package --locked`. If the version is already on crates.io nothing more happens, so running it again is safe.
2. **Publish.** Runs only when `dry_run` is off and the version is not yet published. It waits for approval on the `crates-io` environment, verifies the package again, then runs `cargo publish --no-verify`. That is the only step that sees the token: the dependencies' build scripts run during the verification, so it is done without the token.

One-time setup, by the repository owner:

1. **Create a token** at crates.io under *Account Settings → API Tokens*, with the scopes `publish-new` and `publish-update`. The first publish creates the crate, so the token cannot yet be limited to its name; after that first publish, replace it with a token limited to `vodnik` and `publish-update`.
2. **Add it as a repository secret** named `CARGO_REGISTRY_TOKEN` under *Settings → Secrets and variables → Actions*.
3. **Create the `crates-io` environment** under *Settings → Environments* and add yourself as a required reviewer. Without a reviewer the publish job runs as soon as the check passes.
4. **Check the links in `Cargo.toml`.** `repository`, `homepage` and `documentation` are shown on crates.io for that version for good, so they must point at where the repository lives when you publish.
5. **Run it once as a dry run**, then again with `dry_run` off. The first publish claims the crate name.

crates.io's Trusted Publishing, which uses GitHub's OpenID Connect identity instead of a stored token, would let the workflow drop the secret; it is worth a look once the first release is out.

## Verifying a release

Each archive and the image carry evidence of where they were built, so a download can be checked rather than trusted.

```sh
# A release archive: the checksum, then the build provenance from GitHub.
sha256sum --check vodnik-v0.4.0-x86_64-unknown-linux-gnu.tar.gz.sha256
gh attestation verify vodnik-v0.4.0-x86_64-unknown-linux-gnu.tar.gz --repo vodnik-rs/vodnik

# The container image: the keyless signature, which names this repository's workflow.
cosign verify ghcr.io/vodnik-rs/vodnik:0.4.0 \
  --certificate-identity-regexp '^https://github.com/vodnik-rs/vodnik/\.github/workflows/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# Or the GitHub attestation of the image.
gh attestation verify oci://ghcr.io/vodnik-rs/vodnik:0.4.0 --repo vodnik-rs/vodnik
```

Pin the image by digest (`ghcr.io/vodnik-rs/vodnik@sha256:...`) in anything that matters, since a tag can move.

## Before the first release from a new repository

- **Make the repository public** first. Build attestations are available for public repositories on every plan, but need an Enterprise plan for private ones, and the arm64 runners are free only for public repositories.
- **Publish the container package.** GHCR creates a new package as private the first time it is pushed to. Under the package's settings, set its visibility to public and link it to the repository.
- **Try the pipeline once on a preview tag**, watching each job. The steps that are plain shell have been run locally, but the workflow as a whole can only run on GitHub.

## Local checks

The scripts are plain Bash and have their own tests, which run in `make ci`:

```sh
make test-scripts
```

They build throwaway git repositories and check the version bump for every commit type, the ignored pre-release tags, merge commits, the changelog sections, and the version stamping.

## Limits worth knowing

- The workflows use only `GITHUB_TOKEN` and the marketplace actions they name. Tags pushed with that token do not start other workflows, which is why the release is a separate manual run, and why the image is published by a workflow the release calls rather than by a `release` event.
- The Linux binaries are dynamically linked against glibc. A static musl build is not provided, because the TLS provider compiles C code that makes that build harder; the container image is built on Debian to match its distroless runtime.
- Nothing publishes to crates.io on its own. The `Publish crate` workflow is a manual run that only packages unless you turn `dry_run` off, because publishing a crate cannot be undone, only yanked.
- macOS and Windows binaries are not built.
- The tag, release, container, and crate workflows have not been run on GitHub yet; the scripts and shell steps they call have been run locally. For the crate workflow that means `cargo package` and the crates.io existence check.
