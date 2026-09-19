#!/usr/bin/env bash
# Sets the package version in Cargo.toml and Cargo.lock so a release build reports its tag.
#
# Usage: stamp-version.sh X.Y.Z     (run from the repository root)
set -euo pipefail

version="${1:?usage: stamp-version.sh X.Y.Z}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "stamp-version: '$version' is not X.Y.Z" >&2
  exit 1
fi
name="$(sed -n 's/^name *= *"\(.*\)".*/\1/p' Cargo.toml | head -n 1)"

# The first top-level `version =` line in Cargo.toml is the package's.
sed -i "0,/^version *= *\".*\"/s//version = \"$version\"/" Cargo.toml
# In Cargo.lock, the version line right after this package's name.
awk -v name="$name" -v version="$version" '
  $0 == "name = \"" name "\"" { print; getline; sub(/"[^"]*"/, "\"" version "\""); print; next }
  { print }
' Cargo.lock >Cargo.lock.new
mv Cargo.lock.new Cargo.lock
