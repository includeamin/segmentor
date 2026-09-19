#!/usr/bin/env bash
# Tests the release scripts against throwaway git repositories.
set -euo pipefail

scripts="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

check() { # description expected actual
  if [[ "$2" == "$3" ]]; then
    printf 'ok   %s\n' "$1"
  else
    printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$1" "$2" "$3"
    failures=$((failures + 1))
  fi
}

new_repo() {
  rm -rf "$work/repo" && mkdir "$work/repo" && cd "$work/repo"
  git init -q -b main
  git config user.email t@example.com && git config user.name tester
  printf '[package]\nname = "demo"\nversion = "0.1.0"\n\n[dependencies]\nfoo = { version = "9.9.9" }\n' >Cargo.toml
  printf '[[package]]\nname = "demo"\nversion = "0.1.0"\n\n[[package]]\nname = "foo"\nversion = "9.9.9"\n' >Cargo.lock
  git add . && git commit -q -m "chore: initial"
}
commit() { # subject [body]
  echo "$RANDOM" >>file.txt && git add file.txt
  if [[ -n "${2-}" ]]; then git commit -q -m "$1" -m "$2"; else git commit -q -m "$1"; fi
}
field() { grep "^$1=" <<<"$2" | head -n 1 | cut -d= -f2-; }
next() { "$scripts/next-version.sh" "${1:-HEAD}"; }

# --- next-version.sh ---------------------------------------------------------------------
new_repo
out="$(next)"
check "first release uses the Cargo.toml version" "true v0.1.0 initial" "$(field should_tag "$out") $(field tag "$out") $(field bump "$out")"

git tag v0.1.0
commit "docs: explain things"; commit "chore: bump"; commit "ci: tweak"
out="$(next)"
check "docs, chore, and ci alone do not release" "false none" "$(field should_tag "$out") $(field bump "$out")"

commit "fix(parser): handle empty box"
out="$(next)"
check "fix is a patch" "v0.1.1 patch" "$(field tag "$out") $(field bump "$out")"

commit "feat(http): add endpoint"
out="$(next)"
check "feat beats fix and is a minor" "v0.2.0 minor" "$(field tag "$out") $(field bump "$out")"

commit "refactor!: drop the old config"
out="$(next)"
check "breaking on 0.x is a minor bump, not a major" "v0.2.0 minor" "$(field tag "$out") $(field bump "$out")"

git tag v1.4.2
commit "perf: faster parse"
out="$(next)"
check "perf is a patch" "v1.4.3 patch" "$(field tag "$out") $(field bump "$out")"

commit "feat: another" "BREAKING CHANGE: the wire format changed"
out="$(next)"
check "a BREAKING CHANGE footer is a major on 1.x" "v2.0.0 major" "$(field tag "$out") $(field bump "$out")"

git tag v2.0.0
commit "fix!: rename a flag"
out="$(next)"
check "the ! marker is a major on 1.x and later" "v3.0.0 major" "$(field tag "$out") $(field bump "$out")"

git tag v3.0.0
out="$(next)"
check "an already tagged commit is not tagged again" "false" "$(field should_tag "$out")"

commit "Update readme without a conventional prefix"
out="$(next)"
check "non-conventional commits do not release" "false" "$(field should_tag "$out")"

git tag v3.1.0-rc.1
commit "fix: real fix"
out="$(next)"
check "pre-release tags are ignored as the baseline" "v3.0.1" "$(field tag "$out")"

git checkout -q -b topic
commit "feat: on a branch"
git checkout -q main
git merge -q --no-ff topic -m "Merge pull request #7 from owner/topic"
out="$(next)"
check "commits behind a merge commit are counted, the merge itself is not" "v3.1.0 minor" "$(field tag "$out") $(field bump "$out")"

# --- changelog.sh ------------------------------------------------------------------------
new_repo
git tag v0.1.0
commit "feat(http): add ready endpoint (#12)"
commit "fix: stop leaking paths"
commit "docs: describe the mapper"
commit "perf(mp4): buffer the parser"
commit "ci: add a workflow"
commit "Something unstructured"
commit "refactor(api)!: rename limits" "BREAKING CHANGE: limits.max_startup_parses is now limits.max_loads"
notes="$("$scripts/changelog.sh" v0.1.0 HEAD https://github.com/o/r)"
has() { grep -qF -- "$1" <<<"$notes" && echo yes || echo no; }
check "changelog has a breaking section" yes "$(has '### ⚠ Breaking changes')"
check "changelog explains the breaking change" yes "$(has 'limits.max_startup_parses is now limits.max_loads')"
check "changelog groups features" yes "$(has '### Features')"
check "changelog renders scope and PR reference" yes "$(has '- **http:** add ready endpoint (#12)')"
check "changelog groups fixes" yes "$(has '### Bug fixes')"
check "changelog groups performance" yes "$(has '### Performance')"
check "changelog groups documentation" yes "$(has '### Documentation')"
check "changelog groups build and ci" yes "$(has '### Build and CI')"
check "changelog keeps non-conventional commits" yes "$(has '### Other changes')"
check "changelog links commits" yes "$(has '](https://github.com/o/r/commit/')"
check "changelog links the comparison" yes "$(has '**Full changelog**: https://github.com/o/r/compare/v0.1.0...HEAD')"
check "features come before fixes" yes "$(awk '/### Features/{f=NR} /### Bug fixes/{b=NR} END{print (f && b && f<b) ? "yes" : "no"}' <<<"$notes")"
check "the chore that opened the repo is outside the range" no "$(has 'initial')"

new_repo
check "an empty range says so" "No user-facing changes." "$("$scripts/changelog.sh" HEAD HEAD | head -n 1)"

# --- stamp-version.sh --------------------------------------------------------------------
new_repo
"$scripts/stamp-version.sh" 4.5.6
check "stamp updates the package version in Cargo.toml" 'version = "4.5.6"' "$(grep -m1 '^version' Cargo.toml)"
check "stamp leaves dependency versions alone" 'foo = { version = "9.9.9" }' "$(grep '^foo' Cargo.toml)"
check "stamp updates only this package in Cargo.lock" "4.5.6 9.9.9" "$(awk -F'"' '/^version/{printf "%s%s", sep, $2; sep=" "}' Cargo.lock)"
if "$scripts/stamp-version.sh" not-a-version 2>/dev/null; then check "stamp rejects a bad version" rejected accepted; else check "stamp rejects a bad version" rejected rejected; fi

if ((failures > 0)); then
  printf '\n%d check(s) failed\n' "$failures"
  exit 1
fi
printf '\nall release script checks passed\n'
