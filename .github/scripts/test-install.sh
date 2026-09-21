#!/usr/bin/env bash
# Tests install.sh against a fake release in a local directory, so nothing touches the network.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
script="$root/install.sh"
work="$(mktemp -d)"
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT
failures=0

check() { # description expected actual
  if [[ "$2" == "$3" ]]; then
    printf 'ok   %s\n' "$1"
  else
    printf 'FAIL %s\n  expected: %s\n  actual:   %s\n' "$1" "$2" "$3"
    failures=$((failures + 1))
  fi
}

contains() { # description needle haystack
  if [[ "$3" == *"$2"* ]]; then
    printf 'ok   %s\n' "$1"
  else
    printf 'FAIL %s\n  expected to contain: %s\n  actual: %s\n' "$1" "$2" "$3"
    failures=$((failures + 1))
  fi
}

# A release for both architectures, so the test passes on either. The "binary" is a script that
# says which version it is.
releases="$work/releases/download"
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  name="segmentor-v9.9.9-$target"
  mkdir -p "$work/build/$name" "$releases/v9.9.9"
  printf '#!/bin/sh\necho "segmentor 9.9.9 (%s)"\n' "$target" >"$work/build/$name/segmentor"
  chmod +x "$work/build/$name/segmentor"
  tar -C "$work/build" -czf "$releases/v9.9.9/$name.tar.gz" "$name"
  (cd "$releases/v9.9.9" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
done
# `latest` redirects to the newest tag; a file URL ending in the tag stands in for that.
mkdir -p "$work/releases/latest"
: >"$work/releases/latest/v9.9.9"

export SEGMENTOR_DOWNLOAD_BASE="file://$releases"
export SEGMENTOR_LATEST_URL="file://$work/releases/latest/v9.9.9"
export HOME="$work/home"
mkdir -p "$HOME"

run() { # runs install.sh, printing its output and exit status on the last line
  local output status=0
  output="$(sh "$script" "$@" 2>&1)" || status=$?
  printf '%s\nexit=%s' "$output" "$status"
}

# -- a plain install -------------------------------------------------------------------------
out="$(run --version v9.9.9 --prefix "$work/bin")"
contains "installs a pinned version" "Installed $work/bin/segmentor" "$out"
contains "exit status is zero" "exit=0" "$out"
contains "checks the checksum" "Checksum verified." "$out"
contains "the installed file runs" "segmentor 9.9.9" "$("$work/bin/segmentor")"
check "the installed file is executable" "yes" "$([[ -x "$work/bin/segmentor" ]] && echo yes || echo no)"
check "no temporary file is left behind" "segmentor" "$(ls "$work/bin")"

out="$(run --version v9.9.9 --prefix "$work/bin")"
contains "installing over an existing binary works" "exit=0" "$out"

out="$(run --prefix "$work/latest-bin")"
contains "resolves the latest release" "segmentor v9.9.9" "$out"
contains "the latest install succeeds" "exit=0" "$out"

out="$(run --version v9.9.9)"
contains "defaults to ~/.local/bin for an ordinary user" "$HOME/.local/bin/segmentor" "$out"
contains "warns when the prefix is not on PATH" "is not on your PATH" "$out"

# -- refusing to install ---------------------------------------------------------------------
# Corrupt the archive after its checksum was written.
cp -r "$releases" "$work/tampered"
name="segmentor-v9.9.9-$(case "$(uname -m)" in aarch64 | arm64) echo aarch64 ;; *) echo x86_64 ;; esac)-unknown-linux-gnu"
printf 'tampered' >>"$work/tampered/v9.9.9/$name.tar.gz"
out="$(SEGMENTOR_DOWNLOAD_BASE="file://$work/tampered" run --version v9.9.9 --prefix "$work/bad")"
contains "rejects a checksum mismatch" "checksum mismatch" "$out"
contains "a mismatch is a failure" "exit=1" "$out"
check "a mismatch installs nothing" "no" "$([[ -e "$work/bad/segmentor" ]] && echo yes || echo no)"

out="$(run --version v1.2.3 --prefix "$work/none")"
contains "a release that does not exist is a clear error" "could not download" "$out"
check "and installs nothing" "no" "$([[ -e "$work/none/segmentor" ]] && echo yes || echo no)"

out="$(run --version latest --prefix "$work/x")"
contains "rejects a version that is not vX.Y.Z" "must look like v1.2.3" "$out"
out="$(run --bogus)"
contains "rejects an unknown option" "unknown option" "$out"
out="$(run --version)"
contains "rejects an option missing its value" "needs a value" "$out"

# -- platforms it does not build for ---------------------------------------------------------
# The stubs are shell scripts whose text contains $1, so the single quotes are deliberate.
fake_uname="$work/fake-uname"
mkdir -p "$fake_uname"
# shellcheck disable=SC2016
printf '#!/bin/sh\nif [ "$1" = "-s" ]; then echo Darwin; else echo arm64; fi\n' >"$fake_uname/uname"
chmod +x "$fake_uname/uname"
out="$(PATH="$fake_uname:$PATH" run --version v9.9.9 --prefix "$work/mac")"
contains "explains that macOS has no binary" "Linux only" "$out"
contains "and points at the container" "container image" "$out"

# shellcheck disable=SC2016
printf '#!/bin/sh\nif [ "$1" = "-s" ]; then echo Linux; else echo armv7l; fi\n' >"$fake_uname/uname"
out="$(PATH="$fake_uname:$PATH" run --version v9.9.9 --prefix "$work/arm32")"
contains "explains an unsupported architecture" "no release binary for the armv7l" "$out"

# -- the prefix ------------------------------------------------------------------------------
if [[ "$(id -u)" -ne 0 ]]; then
  mkdir -p "$work/readonly" && chmod 555 "$work/readonly"
  out="$(run --version v9.9.9 --prefix "$work/readonly")"
  contains "an unwritable prefix is an error, not a sudo prompt" "cannot write to" "$out"
fi

# -- dry run ---------------------------------------------------------------------------------
out="$(SEGMENTOR_DOWNLOAD_BASE="file:///does/not/exist" run --dry-run --prefix "$work/dry")"
contains "a dry run says what it would do" "Would download" "$out"
contains "and succeeds without a network" "exit=0" "$out"
check "and installs nothing" "no" "$([[ -e "$work/dry" ]] && echo yes || echo no)"

# -- build provenance ------------------------------------------------------------------------
stubs="$work/fake-gh"
mkdir -p "$stubs"
printf '#!/bin/sh\nexit 0\n' >"$stubs/gh"; chmod +x "$stubs/gh"
out="$(PATH="$stubs:$PATH" run --version v9.9.9 --prefix "$work/attest-ok")"
contains "reports verified provenance" "Build provenance verified." "$out"

printf '#!/bin/sh\nexit 1\n' >"$stubs/gh"
out="$(PATH="$stubs:$PATH" run --version v9.9.9 --prefix "$work/attest-warn")"
contains "a failed provenance check is a warning by default" "Warning: could not verify the build provenance" "$out"
contains "and the install still succeeds" "exit=0" "$out"
out="$(PATH="$stubs:$PATH" run --version v9.9.9 --prefix "$work/attest-strict" --require-attestation)"
contains "--require-attestation makes it fatal" "--require-attestation was given" "$out"
check "and installs nothing" "no" "$([[ -e "$work/attest-strict/segmentor" ]] && echo yes || echo no)"

# Without gh at all: a PATH holding only the tools the script uses.
tools="$work/tools"
mkdir -p "$tools"
for tool in sh bash cat cp chmod cut curl id ls mkdir mktemp mv rm sed sha256sum tail tar uname; do
  path="$(command -v "$tool")"
  ln -sf "$path" "$tools/$tool"
done
out="$(PATH="$tools" run --version v9.9.9 --prefix "$work/nogh")"
contains "notes when gh is not installed" "gh tool is not installed" "$out"
out="$(PATH="$tools" run --version v9.9.9 --prefix "$work/nogh-strict" --require-attestation)"
contains "--require-attestation needs gh" "needs the gh tool" "$out"

if [[ "$failures" -gt 0 ]]; then
  printf '\n%d install.sh test(s) failed\n' "$failures"
  exit 1
fi
printf '\nAll install.sh tests passed\n'
