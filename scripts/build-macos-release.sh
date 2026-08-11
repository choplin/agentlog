#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
host=$(rustc -vV | sed -n 's/^host: //p')

if [ "$(uname -s)" != "Darwin" ] || [ -z "$host" ]; then
  echo "A native macOS Rust toolchain is required." >&2
  exit 1
fi

case "$host" in
  aarch64-apple-darwin) expected_arch=arm64 ;;
  x86_64-apple-darwin) expected_arch=x86_64 ;;
  *)
    echo "The active Rust host is not a supported macOS target: $host" >&2
    exit 1
    ;;
esac

dist_dir="$repo_root/dist"
if [ -L "$dist_dir" ]; then
  echo "Refusing to write through a symlinked dist directory: $dist_dir" >&2
  exit 1
fi
mkdir -p "$dist_dir"
if [ ! -d "$dist_dir" ]; then
  echo "The release output path is not a directory: $dist_dir" >&2
  exit 1
fi

staging_root=$(mktemp -d "$dist_dir/.agentlog-release.XXXXXX")
trap 'rm -rf -- "$staging_root"' EXIT HUP INT TERM
target_dir="$staging_root/cargo-target"

CARGO_TARGET_DIR="$target_dir" cargo build \
  --manifest-path "$repo_root/Cargo.toml" \
  --release \
  --locked \
  --target "$host"

binary="$target_dir/$host/release/agentlog"
binary_description=$(file -b "$binary")
case "$binary_description" in
  *"$expected_arch"*) ;;
  *)
    echo "The release binary is not native for $host: $binary_description" >&2
    exit 1
    ;;
esac

version=$("$binary" --version | awk 'NR == 1 { print $2 }')
if [ -z "$version" ]; then
  echo "Could not determine the Agentlog version." >&2
  exit 1
fi

bundle_name="agentlog-$version-$host"
bundle_dir="$staging_root/$bundle_name"
staged_archive="$staging_root/$bundle_name.tar.gz"
archive="$dist_dir/$bundle_name.tar.gz"

mkdir -p "$bundle_dir"
install -m 0755 "$binary" "$bundle_dir/agentlog"
install -m 0755 "$repo_root/scripts/install-macos-release.sh" "$bundle_dir/install.sh"
install -m 0644 "$repo_root/README.md" "$bundle_dir/README.md"
COPYFILE_DISABLE=1 tar -czf "$staged_archive" -C "$staging_root" "$bundle_name"

if [ -L "$archive" ]; then
  echo "Refusing to replace a symlinked release archive: $archive" >&2
  exit 1
fi
if [ -e "$archive" ] && [ ! -f "$archive" ]; then
  echo "Refusing to replace a non-file release archive path: $archive" >&2
  exit 1
fi
mv -f -- "$staged_archive" "$archive"

printf '%s\n' "$archive"
