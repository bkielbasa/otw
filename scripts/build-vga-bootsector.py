#!/usr/bin/env python3
"""Build a 512-byte real-mode boot sector that exercises VGA mode 13h.

The sector sets 320x200x256-color mode, rewrites DAC entry 0x2A to red,
draws three test pixels into the 0xA0000 framebuffer, and exits via port 0xF4.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "boot" / "OTWVGA.BIN"


def u16(value: int) -> bytes:
    return struct.pack("<H", value & 0xFFFF)


def build_boot_sector() -> bytes:
    code = bytearray()

    # INT 10h/AH=00h, AL=13h: select 320x200x256-color graphics mode.
    code += b"\xb8" + u16(0x0013)  # mov ax, 0x0013
    code += b"\xcd\x10"            # int 0x10

    # DAC entry 0x2A = bright red.
    code += b"\xba" + u16(0x03C8)  # mov dx, 0x03c8
    code += b"\xb0\x2a\xee"        # mov al, 0x2a; out dx, al
    code += b"\xba" + u16(0x03C9)  # mov dx, 0x03c9
    code += b"\xb0\xff\xee"        # mov al, 0xff; out dx, al
    code += b"\xb0\x00\xee"        # mov al, 0x00; out dx, al
    code += b"\xb0\x00\xee"        # mov al, 0x00; out dx, al

    # ES:0 = 0xA0000, the mode-13h framebuffer.
    code += b"\xb8" + u16(0xA000)  # mov ax, 0xa000
    code += b"\x8e\xc0"            # mov es, ax
    code += b"\xbf" + u16(0x0000)  # mov di, 0
    code += b"\x26\xc6\x05\x2a"    # mov byte es:[di], 0x2a

    code += b"\xbf" + u16(0xF960)  # row 199, column 160
    code += b"\x26\xc6\x05\x10"    # mov byte es:[di], 0x10

    code += b"\xbf" + u16(0xF9FF)  # final pixel
    code += b"\x26\xc6\x05\xff"    # mov byte es:[di], 0xff

    code += b"\xba" + u16(0x00F4)  # mov dx, 0x00f4
    code += b"\xb0\x00\xee"        # mov al, 0; out dx, al
    code += b"\xf4"                # hlt

    image = bytearray(512)
    image[: len(code)] = code
    image[510:512] = b"\x55\xaa"
    return bytes(image)


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    image = build_boot_sector()
    OUTPUT.write_bytes(image)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(image)} bytes)")


if __name__ == "__main__":
    main()
