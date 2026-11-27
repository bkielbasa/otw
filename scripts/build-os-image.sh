#!/usr/bin/env bash
# Build target/os/otw.img: a bootable (UEFI) disk image of OTW running on
# the machine by itself (baremetal/: no other operating system underneath).
# Drive C: is a second partition (OTW-C) filled from OTW_DRIVE_C (default
# ~/.otw/C); OTW_DRIVE_C= (empty) leaves it out. It has OTW_DRIVE_C_FREE MiB
# (default 2048) free for what is saved and downloaded there: the kernel
# writes it to the disk.
# Built again over an existing image, the image keeps its drive C:: the
# kernel's partition is made again, OTW's own files on C: (C:\OTW, the
# fonts) are brought up to date, and files from OTW_DRIVE_C that are not
# on C: yet are added; nothing saved, downloaded or installed there is
# lost. OTW_FRESH=1 makes the whole image anew instead.
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
# TrueType fonts for programs (DejaVu and Liberation, free fonts) from this
# machine, in C:\WINDOWS\FONTS.
mkdir -p "$EXTRA/WINDOWS/FONTS"
for dir in /usr/share/fonts/truetype/dejavu /usr/share/fonts/truetype/liberation /usr/share/fonts/truetype/liberation2; do
  [[ -d "$dir" ]] && cp "$dir"/*.ttf "$EXTRA/WINDOWS/FONTS/" 2>/dev/null || true
done
# The App Center (apps/app-center), built for Windows when the toolchain
# is there (scripts/build-app-center.sh --fetch installs it), in
# C:\Program Files\App Center with a shortcut on the desktop and in the
# Start menu. OTW_APP_CENTER=0 leaves it out.
(cd "$ROOT" && scripts/cargo.sh build --quiet --bin otw-disk)
APPS="$ROOT/apps/app-center/target/x86_64-pc-windows-gnullvm/release"
if [[ "${OTW_APP_CENTER:-1}" != 0 ]] && "$ROOT/scripts/build-app-center.sh" >/dev/null; then
  mkdir -p "$EXTRA/Program Files/App Center" "$EXTRA/WINDOWS/Desktop" "$EXTRA/WINDOWS/Start Menu/Programs"
  cp "$APPS"/*.exe "$EXTRA/Program Files/App Center/"
  if [[ -f "$APPS/app-center-gui.exe" ]]; then
    for at in "$EXTRA/WINDOWS/Desktop" "$EXTRA/WINDOWS/Start Menu/Programs"; do
      "$ROOT/target/debug/otw-disk" link "$at/App Center.lnk" 'C:\Program Files\App Center\app-center-gui.exe' "App Center" >/dev/null
    done
  fi
else
  echo "==> the App Center is left out (no Windows toolchain: scripts/build-app-center.sh --fetch)"
fi
DRIVE_C="${OTW_DRIVE_C-$HOME/.otw/C}"
FREE="${OTW_DRIVE_C_FREE:-2048}"
IMAGE="${OS_IMAGE:-$OUT/otw.img}"

# An image with a drive C: already: keep it.
if [[ "${OTW_FRESH:-0}" != 1 && -f "$IMAGE" ]] && grep -qa "O.T.W.-.C" <(head -c 32768 "$IMAGE"); then
  # Not while a VM has it open (QEMU locks its disk images).
  if ! python3 -I -c 'import fcntl, sys
f = open(sys.argv[1], "r+b")
try:
    fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    sys.exit(1)' "$IMAGE"; then
    echo "$IMAGE is in use (a VM is running on it): stop it first" >&2
    exit 1
  fi
  echo "==> updating $IMAGE (drive C: is kept; OTW_FRESH=1 to start anew)"
  python3 -I "$ROOT/scripts/make-disk.py" --update "$IMAGE" "${partitions[0]}"
  "$ROOT/target/debug/otw-disk" sync "$IMAGE" OTW-C --replace "$EXTRA"
  if [[ -n "$DRIVE_C" && -d "$DRIVE_C" ]]; then
    "$ROOT/target/debug/otw-disk" sync "$IMAGE" OTW-C "$DRIVE_C"
  fi
  exit 0
fi

if [[ -n "$DRIVE_C" && -d "$DRIVE_C" ]]; then
  partitions+=("data:OTW-C:auto+$FREE:$DRIVE_C+$EXTRA")
else
  partitions+=("data:OTW-C:auto+$FREE:$EXTRA")
fi
echo "==> making the disk"
python3 -I "$ROOT/scripts/make-disk.py" "$IMAGE" "${partitions[@]}"
