#!/usr/bin/env bash
# Build the kcd-indicator .deb. Run on Debian (CI uses debian:trixie) with
# cargo, libgtk-4-dev, libadwaita-1-dev, debhelper and desktop-file-utils.
#
# Usage:
#   packaging/debian/build.sh
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

echo "==> Building release binary"
( cd "$REPO_ROOT" && cargo build --release --locked )

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -1)"
stage="$(mktemp -d)/kcd-indicator-$version"
mkdir -p "$stage/debian/source"
cp "$SCRIPT_DIR"/{control,rules,copyright} "$stage/debian/"
cp "$SCRIPT_DIR/source/format" "$stage/debian/source/"
install -m755 "$REPO_ROOT/target/release/kcd-indicator" "$stage/"
install -m644 "$REPO_ROOT/kcd-indicator.desktop" "$stage/"
cat > "$stage/debian/changelog" <<CHANGELOG
kcd-indicator ($version) unstable; urgency=medium

  * Release $version.

 -- Rishabh672003 <53911515+Rishabh672003@users.noreply.github.com>  $(date -R)
CHANGELOG

echo "==> Running dpkg-buildpackage"
( cd "$stage" && dpkg-buildpackage -b -us -uc )
cp "$stage"/../*.deb "$SCRIPT_DIR/"
echo "==> Done: $(ls "$SCRIPT_DIR"/*.deb)"
