#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
archive=$("$repo_root/scripts/build-macos-release.sh")
smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/agentlog-install-smoke.XXXXXX")
trap 'rm -rf -- "$smoke_root"' EXIT HUP INT TERM

COPYFILE_DISABLE=1 tar -xzf "$archive" -C "$smoke_root"
bundle_name=$(basename "$archive" .tar.gz)
bundle_dir="$smoke_root/$bundle_name"
prefix="$smoke_root/prefix"
binary_description=$(file -b "$bundle_dir/agentlog")
host=$(rustc -vV | sed -n 's/^host: //p')

case "$host" in
  aarch64-apple-darwin) expected_arch=arm64 ;;
  x86_64-apple-darwin) expected_arch=x86_64 ;;
  *)
    echo "The active Rust host is not supported: $host" >&2
    exit 1
    ;;
esac
case "$bundle_name" in
  *"-$host") ;;
  *)
    echo "The archive target does not match the active Rust host: $bundle_name" >&2
    exit 1
    ;;
esac
case "$binary_description" in
  *"$expected_arch"*) ;;
  *)
    echo "The installed binary is not native for $expected_arch: $binary_description" >&2
    exit 1
    ;;
esac

"$bundle_dir/install.sh" "$prefix"
"$prefix/bin/agentlog" --help >/dev/null
"$prefix/bin/agentlog" --home "$smoke_root/home" paths --json >/dev/null

printf 'Install smoke passed for %s\n' "$(basename "$archive")"
