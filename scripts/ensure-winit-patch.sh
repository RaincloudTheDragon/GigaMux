#!/usr/bin/env bash
# Materialize crates.io winit into target/patch and apply patches/winit+VERSION.patch (smb:// → kio-fuse).
# Idempotent. Must run before `cargo build` after clone / `cargo clean` / winit version bump
# (Cargo resolves [patch.crates-io] paths before any build.rs can create them).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VER="$(awk '
  $0 == "name = \"winit\"" { in_winit=1; next }
  in_winit && /^version = / {
    gsub(/"/, "", $3); print $3; exit
  }
' Cargo.lock)"
if [[ -z "${VER}" ]]; then
  echo "ensure-winit-patch: could not read winit version from Cargo.lock" >&2
  exit 1
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
DEST="$TARGET_DIR/patch/winit-${VER}"
PATCH="$ROOT/patches/winit+${VER}.patch"
MARKER="$DEST/.gigamux-patched"

if [[ ! -f "$PATCH" ]]; then
  echo "ensure-winit-patch: missing $PATCH (refresh the diff when bumping winit)" >&2
  exit 1
fi

if [[ -f "$MARKER" ]]; then
  exit 0
fi

# Download the crates.io .crate directly (do not call cargo; nested cargo deadlocks under build.rs / locks).
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
curl -fsSL "https://static.crates.io/crates/winit/winit-${VER}.crate" | tar -xz -C "$TMP"
if [[ ! -d "$TMP/winit-${VER}" ]]; then
  echo "ensure-winit-patch: unexpected .crate layout for winit-${VER}" >&2
  exit 1
fi

rm -rf "$DEST"
mkdir -p "$(dirname "$DEST")"
mv "$TMP/winit-${VER}" "$DEST"

if ! (cd "$DEST" && git apply -p1 "$PATCH"); then
  echo "ensure-winit-patch: git apply failed for $PATCH onto winit-${VER}" >&2
  exit 1
fi
touch "$MARKER"
echo "ensure-winit-patch: applied $PATCH → $DEST"
