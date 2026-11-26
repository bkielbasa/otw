#!/usr/bin/env bash
# Build target/os/otw.img: a bootable (UEFI) disk image of OTW running on
# the machine by itself (baremetal/: no other operating system underneath).
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

echo "==> making the disk"
python3 -I "$ROOT/scripts/make-disk.py" "$OUT/otw.img" "esp:OTW:40:$ESP"
