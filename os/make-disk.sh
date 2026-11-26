#!/bin/sh
# make-disk.sh ROOTFS GRUB_CFG OUTPUT: a GPT disk with an EFI system partition
# (GRUB, standalone, with GRUB_CFG built in) and ROOTFS as an ext4 partition
# labelled otw-root.
set -eu
rootfs="$1"; grub_cfg="$2"; output="$3"
work="$(mktemp -d)"
mkdir -p "$(dirname "$output")"

# The system partition: what it holds, a quarter more, and 512 MiB to spare.
used_mib=$(( $(du -sxm "$rootfs" | cut -f1) ))
root_mib=$(( used_mib + used_mib / 4 + 512 ))
mkfs.ext4 -q -L otw-root -d "$rootfs" "$work/root.img" "${root_mib}M"

esp_mib=64
grub-mkstandalone -O x86_64-efi -o "$work/BOOTX64.EFI" \
  --modules="part_gpt ext2 search search_label linux normal echo" \
  "boot/grub/grub.cfg=$grub_cfg"
mkfs.vfat -n OTW-EFI -C "$work/esp.img" $(( esp_mib * 1024 )) >/dev/null
mmd -i "$work/esp.img" ::/EFI ::/EFI/BOOT
mcopy -i "$work/esp.img" "$work/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI

total_mib=$(( 1 + esp_mib + root_mib + 1 ))
truncate -s "${total_mib}M" "$output"
sfdisk -q "$output" <<PARTS
label: gpt
start=1MiB, size=${esp_mib}MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=EFI
start=$(( 1 + esp_mib ))MiB, size=${root_mib}MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=otw-root
PARTS
dd if="$work/esp.img" of="$output" bs=1M seek=1 conv=notrunc,sparse status=none
dd if="$work/root.img" of="$output" bs=1M seek=$(( 1 + esp_mib )) conv=notrunc,sparse status=none
rm -rf "$work"
