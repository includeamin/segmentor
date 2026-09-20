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

The workflow then:

1. builds the changelog for that tag from the commits since the previous version tag, grouped into Breaking changes, Features, Bug fixes, Performance, Documentation, Refactoring, Build and CI, Tests, and Other changes, with links to each commit and to the full comparison;
2. checks out the tag, stamps its version into `Cargo.toml` and `Cargo.lock` for the build only, and builds `segmentor` in release mode with `--locked`;
3. packages `segmentor-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` (the binary, `LICENSE`, `README.md`, and `vod.example.toml`) with a SHA-256 file;
4. creates the GitHub release, or **updates it if it already exists** (notes, title, and channel), and uploads the assets.

Running it again for the same tag is safe, so a preview can be promoted to latest by re-running with `channel = latest`. A preview release is titled `vX.Y.Z (preview)`.

## Local checks

The scripts are plain Bash and have their own tests, which run in `make ci`:

```sh
make test-scripts
```

They build throwaway git repositories and check the version bump for every commit type, the ignored pre-release tags, merge commits, the changelog sections, and the version stamping.

## Limits worth knowing

- The workflows use only `GITHUB_TOKEN` and preinstalled tools. Tags pushed with that token do not start other workflows, which is why the release is a separate manual run.
- Only a Linux x86-64 binary is attached. Add a matrix to `release.yml` for more targets.
- No container image is published. Adding one needs a registry choice and credentials.
- The tag and release workflows have not been run on GitHub yet; the scripts they call are tested locally.
