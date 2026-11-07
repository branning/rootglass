#!/usr/bin/env bash
set -euo pipefail

readonly GHOSTTY_VT_VERSION="1.0.0"
readonly ZIG_VERSION="0.17.0"
readonly ZIG_SHA256="1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026"
readonly GHOSTTY_MINISIGN_PUBLIC_KEY="RWQlAjJC23149WL2sEpT/l0QKy7hMIFhYdQOFy0Z7z7PbneUgvlsnYcV"
readonly ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rootglass-libghostty-vt.XXXXXX")"

cleanup() {
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

for tool in curl minisign sha256sum tar; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'required tool not found: %s\n' "$tool" >&2
    exit 1
  fi
done

download() {
  curl --fail --location --retry 3 --silent --show-error --output "$2" "$1"
}

ghostty_release_url="https://release.files.ghostty.org/libghostty-vt/${GHOSTTY_VT_VERSION}"
download "$ghostty_release_url/SHA256SUMS" "$WORK_DIR/SHA256SUMS"
download "$ghostty_release_url/SHA256SUMS.minisig" "$WORK_DIR/SHA256SUMS.minisig"
download "$ghostty_release_url/libghostty-vt-${GHOSTTY_VT_VERSION}.tar.gz" \
  "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}.tar.gz"
download "$ghostty_release_url/libghostty-vt-${GHOSTTY_VT_VERSION}-deps.tar.gz" \
  "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}-deps.tar.gz"

minisign -V -m "$WORK_DIR/SHA256SUMS" -P "$GHOSTTY_MINISIGN_PUBLIC_KEY"
(
  cd "$WORK_DIR"
  grep -E "  libghostty-vt-${GHOSTTY_VT_VERSION}(-deps)?\\.tar\\.gz$" SHA256SUMS \
    | sha256sum --check --strict
)

zig_archive="zig-x86_64-linux-${ZIG_VERSION}.tar.xz"
download "https://ziglang.org/download/${ZIG_VERSION}/${zig_archive}" "$WORK_DIR/$zig_archive"
printf '%s  %s\n' "$ZIG_SHA256" "$WORK_DIR/$zig_archive" | sha256sum --check --strict
tar -xJf "$WORK_DIR/$zig_archive" -C "$WORK_DIR"
zig_binary="$WORK_DIR/zig-x86_64-linux-${ZIG_VERSION}/zig"

tar -xzf "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}.tar.gz" -C "$WORK_DIR"
tar -xzf "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}-deps.tar.gz" -C "$WORK_DIR"
mkdir "$WORK_DIR/zig-global-cache" "$WORK_DIR/prefix"
(
  cd "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}"
  ZIG_GLOBAL_CACHE_DIR="$WORK_DIR/zig-global-cache" \
    "$zig_binary" build \
      --prefix "$WORK_DIR/prefix" \
      --system "$WORK_DIR/libghostty-vt-${GHOSTTY_VT_VERSION}-deps" \
      -Doptimize=ReleaseFast \
      -Dcpu=baseline
)

GHOSTTY_VT_PREFIX="$WORK_DIR/prefix" \
  cargo test --locked --manifest-path \
    "$ROOT_DIR/prototypes/libghostty-vt-ratatui/Cargo.toml" -- --nocapture
