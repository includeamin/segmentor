#!/bin/sh
# Installs a segmentor release binary for Linux (x86-64 or arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/includeamin/segmentor/main/install.sh | sh
#   sh install.sh --version v0.4.0 --prefix /usr/local/bin
#
# Read it before you run it: it is short. It downloads one release archive and its checksum from
# GitHub, verifies the checksum, and copies one file. It never uses sudo, so an unwritable prefix is
# an error and not a surprise. Where the `gh` tool is installed it also checks the archive's build
# provenance.
#
# The binary is one file. Running it as a service needs a configuration file and, on a server, a
# service manager: see docs/deployment.md, or use the container image instead.
set -eu

REPO="includeamin/segmentor"
# These can be overridden, which is how the script is tested against a local directory.
DOWNLOAD_BASE="${SEGMENTOR_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"
LATEST_URL="${SEGMENTOR_LATEST_URL:-https://github.com/$REPO/releases/latest}"

version=""
prefix=""
require_attestation=0
dry_run=0

usage() {
  cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--prefix DIR] [--require-attestation] [--dry-run]

  --version              The release to install. Default: the latest release.
  --prefix               The directory to put the binary in. Default: /usr/local/bin when run as
                         root, otherwise ~/.local/bin.
  --require-attestation  Fail unless `gh attestation verify` succeeds. Without this flag a failed
                         or unavailable check is only a warning.
  --dry-run              Say what would be done, and download nothing.
EOF
}

say() { printf '%s\n' "$*"; }
fail() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version) [ "$#" -ge 2 ] || fail "--version needs a value"; version="$2"; shift 2 ;;
    --prefix) [ "$#" -ge 2 ] || fail "--prefix needs a value"; prefix="$2"; shift 2 ;;
    --require-attestation) require_attestation=1; shift ;;
    --dry-run) dry_run=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; fail "unknown option: $1" ;;
  esac
done

# Only Linux builds are published. Anywhere else, the container or a source build is the way.
os="$(uname -s)"
[ "$os" = "Linux" ] || fail "release binaries are built for Linux only, and this is $os. Use the container image (ghcr.io/$REPO) or 'cargo install --git https://github.com/$REPO'."

case "$(uname -m)" in
  x86_64 | amd64) target="x86_64-unknown-linux-gnu" ;;
  aarch64 | arm64) target="aarch64-unknown-linux-gnu" ;;
  *) fail "no release binary for the $(uname -m) architecture. Use the container image or 'cargo install --git https://github.com/$REPO'." ;;
esac

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
  resolve() { curl -fsSLI -o /dev/null -w '%{url_effective}' "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
  resolve() { wget -q -S --spider "$1" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1; }
else
  fail "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
  checksum() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
  checksum() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
  fail "sha256sum or shasum is required to verify the download"
fi
command -v tar >/dev/null 2>&1 || fail "tar is required"

if [ -z "$prefix" ]; then
  if [ "$(id -u)" -eq 0 ]; then prefix="/usr/local/bin"; else prefix="${HOME:?HOME is not set}/.local/bin"; fi
fi

if [ -z "$version" ]; then
  # The latest release redirects to .../tag/vX.Y.Z; reading that avoids the API and its rate limit.
  if [ "$dry_run" -eq 1 ]; then
    version="<the latest release>"
  else
    version="$(resolve "$LATEST_URL" | sed 's|.*/||')"
    case "$version" in
      v[0-9]*.[0-9]*.[0-9]*) ;;
      *) fail "could not work out the latest release (got '$version'). Pass --version vX.Y.Z." ;;
    esac
  fi
fi
case "$version" in
  v[0-9]*.[0-9]*.[0-9]* | "<the latest release>") ;;
  *) fail "the version must look like v1.2.3, not '$version'" ;;
esac

name="segmentor-$version-$target"
say "Installing segmentor $version for $target into $prefix"
if [ "$dry_run" -eq 1 ]; then
  say "Would download $DOWNLOAD_BASE/$version/$name.tar.gz and its .sha256, verify it, and install '$prefix/segmentor'."
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

fetch "$DOWNLOAD_BASE/$version/$name.tar.gz" "$work/$name.tar.gz" || fail "could not download $name.tar.gz. Does release $version exist for $target?"
fetch "$DOWNLOAD_BASE/$version/$name.tar.gz.sha256" "$work/$name.tar.gz.sha256" || fail "could not download the checksum file"

expected="$(cut -d ' ' -f 1 "$work/$name.tar.gz.sha256")"
actual="$(checksum "$work/$name.tar.gz")"
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  fail "checksum mismatch for $name.tar.gz (expected '$expected', got '$actual'). Nothing was installed."
fi
say "Checksum verified."

# The checksum only proves the archive matches the file next to it. The attestation ties it to the
# workflow run that built it.
if command -v gh >/dev/null 2>&1; then
  if gh attestation verify "$work/$name.tar.gz" --repo "$REPO" >/dev/null 2>&1; then
    say "Build provenance verified."
  elif [ "$require_attestation" -eq 1 ]; then
    fail "the build provenance could not be verified, and --require-attestation was given"
  else
    say "Warning: could not verify the build provenance (is gh logged in?). Continuing on the checksum alone." >&2
  fi
elif [ "$require_attestation" -eq 1 ]; then
  fail "--require-attestation needs the gh tool, which is not installed"
else
  say "Skipped the build provenance check: the gh tool is not installed."
fi

tar -xzf "$work/$name.tar.gz" -C "$work"
[ -f "$work/$name/segmentor" ] || fail "the archive does not contain segmentor"

mkdir -p "$prefix" 2>/dev/null || true
[ -d "$prefix" ] && [ -w "$prefix" ] || fail "cannot write to $prefix. Choose another with --prefix, or run this as a user that can."
cp "$work/$name/segmentor" "$prefix/segmentor.new"
chmod 0755 "$prefix/segmentor.new"
# Moving into place replaces a running binary safely; copying over it would fail while it runs.
mv -f "$prefix/segmentor.new" "$prefix/segmentor"

say "Installed $prefix/segmentor"
case ":${PATH:-}:" in
  *":$prefix:"*) ;;
  *) say "Note: $prefix is not on your PATH." ;;
esac
say ""
say "Next: write a configuration (the archive's vod.example.toml lists every setting), then run"
say "  segmentor serve --config vod.toml"
say "To run it as a service, see docs/deployment.md."
