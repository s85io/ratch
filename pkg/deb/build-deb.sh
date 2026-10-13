#!/bin/sh
# Build a .deb from the source tree: sh pkg/deb/build-deb.sh
set -e

SRCDIR="$(cd "$(dirname "$0")/../.." && pwd)"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$SRCDIR/Cargo.toml" | head -1)
ARCH=$(dpkg --print-architecture)
PKGDIR="$(mktemp -d)"
trap 'rm -rf "$PKGDIR"' EXIT

make -C "$SRCDIR" install PREFIX=/usr DESTDIR="$PKGDIR"

mkdir -p "$PKGDIR/DEBIAN"
cat > "$PKGDIR/DEBIAN/control" <<CONTROL
Package: ratch
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${ARCH}
Depends: libc6
Maintainer: s85io <noreply@github.com>
Homepage: https://github.com/s85io/ratch
Description: Detachable terminal sessions without terminal emulation
 Keeps a terminal program running after you detach and lets you attach
 again later. Output and input pass through unchanged; history is kept
 and replayed; terminal modes are restored on attach and undone on detach.
CONTROL

dpkg-deb --root-owner-group --build "$PKGDIR" "${SRCDIR}/ratch_${VERSION}_${ARCH}.deb"
echo "Built: ratch_${VERSION}_${ARCH}.deb"
