#!/usr/bin/env python3
"""Build a tiny 512-byte QEMU boot loader for the flat OTW kernel.

QEMU's `loader` device places HELLO.BIN at physical address 0x100000 before
reset. This boot sector switches to 32-bit protected mode, installs flat code
and data descriptors, sets the data segments, and far-jumps to that address.
It is a QEMU-only test loader; the host VM's built-in --kernel path loads the
same flat binary directly without a boot sector.
"""

import struct
import sys
from pathlib import Path

BOOT_ADDR = 0x7C00
PROTECTED_OFFSET = 0x16
GDT_OFFSET = 0x30
GDT_LIMIT = 24 - 1
KERNEL_ENTRY = 0x00100000
DATA_SELECTOR = 0x10
CODE_SELECTOR = 0x08


def put(image: bytearray, offset: int, data: bytes) -> None:
    image[offset : offset + len(data)] = data


def build() -> bytes:
    image = bytearray(512)

    gdt_descriptor_offset = GDT_OFFSET + 24
    real_mode_code = bytes(
        [
            0xFA,  # cli
            0x0F, 0x01, 0x16,  # lgdt [gdt_descriptor]
            (BOOT_ADDR + gdt_descriptor_offset) & 0xFF,
            ((BOOT_ADDR + gdt_descriptor_offset) >> 8) & 0xFF,
            0x0F, 0x20, 0xC0,  # mov eax, cr0
            0x66, 0x0D, 0x01, 0x00, 0x00, 0x00,  # or eax, 1
            0x0F, 0x22, 0xC0,  # mov cr0, eax
            0xEA,  # jmp far 0x08:protected
            (BOOT_ADDR + PROTECTED_OFFSET) & 0xFF,
            ((BOOT_ADDR + PROTECTED_OFFSET) >> 8) & 0xFF,
            CODE_SELECTOR & 0xFF,
            CODE_SELECTOR >> 8,
        ]
    )
    put(image, 0, real_mode_code)

    protected_code = bytes(
        [
            0xB8, DATA_SELECTOR & 0xFF, DATA_SELECTOR >> 8,  # mov ax, 0x10
            0x8E, 0xD8,  # mov ds, ax
            0x8E, 0xC0,  # mov es, ax
            0x8E, 0xE0,  # mov fs, ax
            0x8E, 0xE8,  # mov gs, ax
            0x8E, 0xD0,  # mov ss, ax
            0xEA,  # jmp far 0x08:0x100000
            KERNEL_ENTRY & 0xFF,
            (KERNEL_ENTRY >> 8) & 0xFF,
            (KERNEL_ENTRY >> 16) & 0xFF,
            (KERNEL_ENTRY >> 24) & 0xFF,
            CODE_SELECTOR & 0xFF,
            CODE_SELECTOR >> 8,
        ]
    )
    put(image, PROTECTED_OFFSET, protected_code)

    gdt = (
        b"\x00" * 8
        + bytes([0xFF, 0xFF, 0x00, 0x00, 0x00, 0x9A, 0xCF, 0x00])  # code
        + bytes([0xFF, 0xFF, 0x00, 0x00, 0x00, 0x92, 0xCF, 0x00])  # data
    )
    put(image, GDT_OFFSET, gdt)
    put(
        image,
        gdt_descriptor_offset,
        struct.pack("<HI", GDT_LIMIT, BOOT_ADDR + GDT_OFFSET),
    )
    image[510:512] = b"\x55\xAA"
    return bytes(image)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} OUTPUT.BIN")
    Path(sys.argv[1]).write_bytes(build())
