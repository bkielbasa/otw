#!/usr/bin/env bash
# Build target/os/otw.img: a bootable (UEFI) disk image of OTW running on
# the machine by itself (baremetal/: no other operating system underneath).
# Drive C: is a second partition (OTW-C) filled from OTW_DRIVE_C (default
# ~/.otw/C); OTW_DRIVE_C= (empty) leaves it out.
# Write it to a USB stick or disk for a real machine
# (sudo dd if=target/os/otw.img of=/dev/sdX bs=4M conv=fsync status=progress)
# and boot it with UEFI and Secure Boot off; or try it with `make os`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/target/os"
ESP="$OUT/esp"

echo "==> building the kernel"
(cd "$ROOT/baremetal" && cargo build --release)

rm -rf "$ESP"
mkdir -p "$ESP/EFI/BOOT"
cp "$ROOT/baremetal/target/x86_64-unknown-uefi/release/otw-kernel.efi" "$ESP/EFI/BOOT/BOOTX64.EFI"

partitions=("esp:OTW:40:$ESP")
# OTW's own test programs, in C:\OTW.
EXTRA="$OUT/extra"
rm -rf "$EXTRA"
mkdir -p "$EXTRA/OTW"
cp "$ROOT"/binaries/guests/*.EXE "$EXTRA/OTW/"
DRIVE_C="${OTW_DRIVE_C-$HOME/.otw/C}"
if [[ -n "$DRIVE_C" && -d "$DRIVE_C" ]]; then
  partitions+=("data:OTW-C:auto:$DRIVE_C+$EXTRA")
else
  partitions+=("data:OTW-C:auto:$EXTRA")
fi
echo "==> making the disk"
python3 -I "$ROOT/scripts/make-disk.py" "$OUT/otw.img" "${partitions[@]}"
