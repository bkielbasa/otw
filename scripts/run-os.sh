#!/usr/bin/env bash
# Boot target/os/otw.img (scripts/build-os-image.sh) in QEMU with UEFI, as a
# real machine would. Writes go to the image itself.
# Settings: OS_IMAGE, VM_CPUS (8), VM_MEMORY (8G), VM_DISPLAY (gtk; none),
# VM_MONITOR (unix socket for the QEMU monitor), VM_QMP (unix socket for
# QMP), VM_SERIAL (serial log file).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${OS_IMAGE:-$ROOT/target/os/otw.img}"
OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"
VARS="$ROOT/target/os/efivars.fd"

if [[ ! -f "$IMAGE" ]]; then
  echo "no image at $IMAGE: build it with 'make image'" >&2
  exit 1
fi
[[ -f "$VARS" ]] || cp "$OVMF_VARS" "$VARS"

args=()
[[ -n "${VM_MONITOR:-}" ]] && args+=(-monitor "unix:$VM_MONITOR,server,nowait")
[[ -n "${VM_QMP:-}" ]] && args+=(-qmp "unix:$VM_QMP,server,nowait")
[[ -n "${VM_SERIAL:-}" ]] && args+=(-serial "file:$VM_SERIAL")

exec qemu-system-x86_64 \
  -name OTW \
  -machine q35,accel=kvm \
  -cpu host \
  -smp "${VM_CPUS:-8}" \
  -m "${VM_MEMORY:-8G}" \
  -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
  -drive "if=pflash,format=raw,file=$VARS" \
  -drive "file=$IMAGE,if=virtio,format=raw" \
  -nic user,model=virtio-net-pci \
  -device virtio-vga \
  -device qemu-xhci -device usb-tablet -device usb-kbd \
  -display "${VM_DISPLAY:-gtk}" \
  "${args[@]}"
