#!/usr/bin/env bash
# Boot the OTW real-mode test sector in QEMU with a VNC display by default.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

QEMU="${QEMU:-qemu-system-i386}"
BOOT="${BOOT:-$ROOT/binaries/boot/OTWBOOT.BIN}"
FLOPPY_IMG="${OTW_QEMU_IMAGE:-/tmp/opencode/otw-boot.img}"
QEMU_DISPLAY="${QEMU_DISPLAY:-vnc=:1}"
MONITOR_ADDR="${OTW_QEMU_MONITOR:-127.0.0.1:4444}"

size="$(stat -c '%s' "$BOOT")"
if [[ "$size" != 512 ]]; then
  echo "boot sector must be 512 bytes, got $size: $BOOT" >&2
  exit 1
fi
signature="$(od -An -tx1 -j 510 -N 2 "$BOOT" | tr -d ' \n')"
if [[ "$signature" != 55aa ]]; then
  echo "boot sector is missing the 0x55AA signature: $BOOT" >&2
  exit 1
fi

mkdir -p "$(dirname "$FLOPPY_IMG")"
dd if=/dev/zero of="$FLOPPY_IMG" bs=512 count=2880 status=none
dd if="$BOOT" of="$FLOPPY_IMG" conv=notrunc status=none

read -r -a extra_args <<< "${QEMU_ARGS:-}"
exec "$QEMU" \
  -accel tcg \
  -m 16 \
  -boot order=a \
  -drive "file=$FLOPPY_IMG,format=raw,if=floppy" \
  -display "$QEMU_DISPLAY" \
  -monitor "tcp:$MONITOR_ADDR,server,nowait" \
  -no-reboot \
  -no-shutdown \
  -name "OTW boot sector" \
  "${extra_args[@]}"
