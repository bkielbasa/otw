# syntax=docker/dockerfile:1
# A bootable disk image whose user interface is the OTW desktop, for real
# machines (UEFI; write it to a USB stick or disk) and QEMU alike: Ubuntu's
# generic kernel and firmware and a minimal userland that logs in on tty1 and starts X with
# otw-vm --desktop. Built by scripts/build-os-image.sh (build contexts
# "otw" — otw-vm and the files under os/ — and "drivec", drive C:).

ARG UBUNTU=26.04

# ── The system ──
FROM ubuntu:${UBUNTU} AS os
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      linux-image-generic linux-firmware initramfs-tools systemd systemd-sysv systemd-resolved udev dbus kmod \
      xserver-xorg-core xserver-xorg-legacy xserver-xorg-input-libinput xinit x11-xserver-utils \
      libgl1 libegl1 libgl1-mesa-dri libxkbcommon-x11-0 libxcursor1 libxrandr2 libxi6 \
      fonts-dejavu-core fonts-liberation fonts-noto-core \
      ca-certificates iproute2 sudo less \
 && rm -rf /var/lib/apt/lists/* \
 && kernel="$(ls /boot | sed -n 's/^vmlinuz-//p' | sort -V | tail -1)" \
 && { [ -f "/boot/initrd.img-$kernel" ] || update-initramfs -c -k "$kernel"; } \
 && ln -sf "vmlinuz-$kernel" /boot/vmlinuz \
 && ln -sf "initrd.img-$kernel" /boot/initrd.img \
 && (id -u ubuntu >/dev/null 2>&1 && userdel -r ubuntu || true) \
 && useradd -m -u 1000 -s /bin/bash -G sudo,video,input,audio otw \
 && echo 'otw ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/otw \
 && passwd -d root \
 && systemctl enable systemd-networkd systemd-resolved \
 && systemctl mask systemd-networkd-wait-online.service
COPY --from=otw os/rootfs/ /
COPY --from=otw os/home/ /home/otw/
COPY --from=otw otw-vm /opt/otw/otw-vm
COPY --from=drivec --chown=1000:1000 . /home/otw/.otw/C/
RUN chown -R otw:otw /home/otw && chmod 0755 /opt/otw/otw-vm /opt/otw/desktop.sh

# ── The disk: an EFI system partition with GRUB, and the system as ext4 ──
FROM ubuntu:${UBUNTU} AS disk
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      e2fsprogs dosfstools mtools fdisk grub-efi-amd64-bin grub-common \
 && rm -rf /var/lib/apt/lists/*
COPY --from=os / /rootfs/
COPY --from=otw os/grub.cfg os/make-disk.sh /build/
RUN sh /build/make-disk.sh /rootfs /build/grub.cfg /out/otw.img

FROM scratch
COPY --from=disk /out/otw.img /
