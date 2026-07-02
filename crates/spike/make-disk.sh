#!/bin/bash
# Build an ext4 disk image from an unpacked rootfs directory (WP0 spike).
# Usage: make-disk.sh <rootfs-dir> <output.img> [size]
set -euo pipefail

ROOTFS="${1:?usage: make-disk.sh <rootfs-dir> <output.img> [size]}"
OUT="${2:?usage: make-disk.sh <rootfs-dir> <output.img> [size]}"
SIZE="${3:-2G}"

MKFS=/opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext4
[ -x "$MKFS" ] || { echo "error: mkfs.ext4 not found (brew install e2fsprogs)"; exit 1; }

rm -f "$OUT"
# -d populates the filesystem from the directory; no mount, no root needed.
"$MKFS" -q -F -d "$ROOTFS" -L gotroot "$OUT" "$SIZE"
echo "built $OUT ($SIZE) from $ROOTFS"
