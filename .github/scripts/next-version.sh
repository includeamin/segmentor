#!/usr/bin/env bash
# Decides the next semantic version from Conventional Commits.
#
# Usage: next-version.sh [ref]        (default ref: HEAD; run from the repository root)
#
# Prints key=value lines, suitable for appending to $GITHUB_OUTPUT:
#   should_tag=true|false
#   tag=vX.Y.Z         (only when should_tag=true)
#   version=X.Y.Z      (only when should_tag=true)
#   previous=vA.B.C    (empty for the first release)
#   bump=initial|major|minor|patch|none
#   reason=...         (why nothing is tagged, when should_tag=false)
#
# Rules, applied to the non-merge commits since the newest version tag reachable from ref:
#   feat                          -> minor
#   fix, perf                     -> patch
#   any type with "!" or a "BREAKING CHANGE:" footer -> major (minor while the version is 0.x)
#   everything else (docs, chore, ci, test, refactor, build, style) -> no release
# The highest level wins. With no version tag yet, the first tag is the version in Cargo.toml.
set -euo pipefail

ref="${1:-HEAD}"
semver='^v[0-9]+\.[0-9]+\.[0-9]+$'

emit() { printf '%s=%s\n' "$1" "$2"; }

# A commit that is already tagged is not tagged again (workflow re-runs).
if git tag --points-at "$ref" --list 'v*' | grep -Eq "$semver"; then
  emit should_tag false
  emit bump none
  emit previous ""
  emit reason "$ref already has a version tag"
  exit 0
fi

previous="$(git tag --merged "$ref" --list 'v*' --sort=-v:refname | grep -E "$semver" | head -n 1 || true)"

if [[ -z "$previous" ]]; then
  version="$(sed -n 's/^version *= *"\(.*\)".*/\1/p' Cargo.toml | head -n 1)"
  if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "next-version: cannot read a plain X.Y.Z version from Cargo.toml (got '$version')" >&2
    exit 1
  fi
  emit should_tag true
  emit tag "v$version"
  emit version "$version"
  emit previous ""
  emit bump initial
  exit 0
fi

level=0 # 1 patch, 2 minor, 3 major
breaking_re=$'(^|\n)BREAKING[ -]CHANGE:'
while IFS= read -r -d $'\x1e' record; do
  record="${record#"${record%%[![:space:]]*}"}"
  [[ -z "$record" ]] && continue
  rest="${record#*$'\x1f'}"
  subject="${rest%%$'\x1f'*}"
  body="${rest#*$'\x1f'}"
  commit_level=0
  if [[ "$subject" =~ ^([A-Za-z]+)(\([^\)]*\))?(!)?:\ .+ ]]; then
    case "${BASH_REMATCH[1],,}" in
      feat) commit_level=2 ;;
      fix | perf) commit_level=1 ;;
    esac
    [[ -n "${BASH_REMATCH[3]}" ]] && commit_level=3
  fi
  [[ "$body" =~ $breaking_re ]] && commit_level=3
  ((commit_level > level)) && level=$commit_level
done < <(git log --no-merges --format='%H%x1f%s%x1f%b%x1e' "$previous..$ref")

if ((level == 0)); then
  emit should_tag false
  emit bump none
  emit previous "$previous"
  emit reason "no feat, fix, perf, or breaking commits since $previous"
  exit 0
fi

IFS=. read -r major minor patch <<<"${previous#v}"
if ((level == 3)); then
  if ((major >= 1)); then
    bump=major
  else
    bump=minor
  fi
else
  bump=$([[ $level -eq 2 ]] && echo minor || echo patch)
fi
case "$bump" in
  major) major=$((major + 1)); minor=0; patch=0 ;;
  minor) minor=$((minor + 1)); patch=0 ;;
  patch) patch=$((patch + 1)) ;;
esac
version="$major.$minor.$patch"
emit should_tag true
emit tag "v$version"
emit version "$version"
emit previous "$previous"
emit bump "$bump"
