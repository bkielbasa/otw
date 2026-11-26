#!/usr/bin/env bash
# Build target/os/otw.img: a bootable (UEFI) disk image that starts the OTW
# desktop, for a real machine or QEMU (scripts/run-os.sh). Drive C: (the
# programs, Firefox among them) is copied in from OTW_DRIVE_C (default
# ~/.otw/C). Needs Docker with BuildKit; nothing runs as root on the host.
#
# On a real machine: write the image to a USB stick or disk
# (sudo dd if=target/os/otw.img of=/dev/sdX bs=4M conv=fsync status=progress),
# and boot it with UEFI and Secure Boot off.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DRIVE_C="${OTW_DRIVE_C:-$HOME/.otw/C}"
OUT="$ROOT/target/os"
CONTEXT="$OUT/context"

echo "==> building otw-vm"
"$ROOT/scripts/cargo.sh" build --release --features display

rm -rf "$CONTEXT"
mkdir -p "$CONTEXT" "$DRIVE_C"
cp -a "$ROOT/os" "$CONTEXT/os"
cp "$ROOT/target/release/otw-vm" "$CONTEXT/otw-vm"

echo "==> building the image (drive C: from $DRIVE_C)"
docker buildx build \
  --file "$ROOT/os/Dockerfile" \
  --build-context otw="$CONTEXT" \
  --build-context drivec="$DRIVE_C" \
  --output "type=local,dest=$OUT" \
  "$CONTEXT"
rm -rf "$CONTEXT"
echo "==> $OUT/otw.img ($(du -h --apparent-size "$OUT/otw.img" | cut -f1))"
