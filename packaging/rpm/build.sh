#!/usr/bin/env bash
# Build the kcd-indicator .rpm. Run on Fedora (CI uses fedora:latest) with
# cargo, gtk4-devel, libadwaita-devel, rpm-build and desktop-file-utils.
#
# Usage:
#   packaging/rpm/build.sh
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

echo "==> Building release binary"
( cd "$REPO_ROOT" && cargo build --release --locked )

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)"
top="$(mktemp -d)"
mkdir -p "$top/SOURCES"
install -m755 "$REPO_ROOT/target/release/kcd-indicator" "$top/SOURCES/"
install -m644 "$REPO_ROOT"/{kcd-indicator.desktop,LICENSE,README.md} "$top/SOURCES/"
sed -e "s/@VERSION@/$version/g" -e "s/@DATE@/$(LC_ALL=C date '+%a %b %d %Y')/" \
    "$SCRIPT_DIR/kcd-indicator.spec.in" > "$top/kcd-indicator.spec"

echo "==> Running rpmbuild"
rpmbuild -bb --define "_topdir $top" "$top/kcd-indicator.spec"
cp "$top"/RPMS/*/*.rpm "$SCRIPT_DIR/"
echo "==> Done: $(ls "$SCRIPT_DIR"/*.rpm)"
