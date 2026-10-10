#!/usr/bin/env bash
# Build the release binary and package it as kcd-indicator (.pkg.tar.zst).
#
# Usage:
#   packaging/arch/build.sh            # build binary + package
#   packaging/arch/build.sh --no-build # skip cargo, repackage existing binary
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

if [[ "${1:-}" != "--no-build" ]]; then
    echo "==> Building release binary..."
    ( cd "$REPO_ROOT" && cargo build --release --locked )
fi

if [[ ! -x "$REPO_ROOT/target/release/kcd-indicator" ]]; then
    echo "error: $REPO_ROOT/target/release/kcd-indicator not found (run without --no-build)" >&2
    exit 1
fi

# pkgver follows Cargo.toml so the package version can't drift from the crate.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)"
sed -i "s/^pkgver=.*/pkgver=$version/" "$SCRIPT_DIR/PKGBUILD"

echo "==> Staging files into packaging/arch/"
install -m755 "$REPO_ROOT/target/release/kcd-indicator" "$SCRIPT_DIR/kcd-indicator"
install -m644 "$REPO_ROOT/kcd-indicator.desktop" "$REPO_ROOT/LICENSE" "$REPO_ROOT/README.md" "$SCRIPT_DIR/"

echo "==> Running makepkg"
( cd "$SCRIPT_DIR" && makepkg -f )

pkg="$(ls -t "$SCRIPT_DIR"/*.pkg.tar.zst | head -1)"

echo "==> Verifying packaged binary matches the build"
built="$(sha256sum "$REPO_ROOT/target/release/kcd-indicator" | cut -d' ' -f1)"
packed="$(tar --use-compress-program=unzstd -xO -f "$pkg" usr/bin/kcd-indicator | sha256sum | cut -d' ' -f1)"
if [[ "$built" != "$packed" ]]; then
    echo "error: packaged binary ($packed) != build ($built)" >&2
    exit 1
fi

echo "==> Done: $pkg"
echo "    Install with: sudo pacman -U '$pkg'"
