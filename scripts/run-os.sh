#!/usr/bin/env bash
# Boot target/os/otw.img (scripts/build-os-image.sh) in QEMU with UEFI, as a
# real machine would: the PC's own PS/2 keyboard and mouse, a plain VGA
# card. The kernel logs to the serial port (VM_SERIAL).
# Settings: OS_IMAGE, VM_CPUS (8), VM_MEMORY (8G), VM_DISK (nvme; sata),
# VM_DISPLAY (gtk; none),
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

# The disk: NVMe (VM_DISK=nvme, as in most new PCs) or SATA (VM_DISK=sata).
case "${VM_DISK:-nvme}" in
  sata) disk_args=(-device ide-hd,drive=disk,bus=ide.0,bootindex=0) ;;
  *) disk_args=(-device nvme,drive=disk,serial=OTW0001,bootindex=0) ;;
esac
# KVM where this user may use it; emulated (slower) otherwise.
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
  cpu_args=(-machine q35,accel=kvm -cpu host)
else
  echo "no access to /dev/kvm: emulating the CPU (slow)" >&2
  cpu_args=(-machine q35,accel=tcg -cpu max)
fi
# A window where there is a desktop to show it on (GTK, else SDL); a VNC
# server (vnc://localhost:5901) otherwise.
display="${VM_DISPLAY:-}"
if [[ -z "$display" ]]; then
  if [[ -n "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]]; then
    display=gtk
  else
    display=vnc=127.0.0.1:1
    echo "no desktop session here: the screen is at vnc://127.0.0.1:5901 (VM_DISPLAY=gtk or sdl to choose)" >&2
  fi
fi
display_args=(-display "$display")
args=()
[[ -n "${VM_MONITOR:-}" ]] && args+=(-monitor "unix:$VM_MONITOR,server,nowait")
[[ -n "${VM_QMP:-}" ]] && args+=(-qmp "unix:$VM_QMP,server,nowait")
[[ -n "${VM_PIDFILE:-}" ]] && args+=(-pidfile "$VM_PIDFILE")
[[ -n "${VM_SERIAL:-}" ]] && args+=(-serial "file:$VM_SERIAL")

exec qemu-system-x86_64 \
  -name OTW \
  "${cpu_args[@]}" \
  -smp "${VM_CPUS:-8}" \
  -m "${VM_MEMORY:-8G}" \
  -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE" \
  -drive "if=pflash,format=raw,file=$VARS" \
  -drive "file=$IMAGE,if=none,id=disk,format=raw" \
  "${disk_args[@]}" \
  -nic user,model=virtio-net-pci \
  -vga std \
  "${display_args[@]}" \
  "${args[@]}"
