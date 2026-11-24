#!/usr/bin/env python3
"""Build a tiny freestanding 32-bit x86 kernel image.

The image is a flat binary loaded at 0x00100000. It assumes the host has
installed a flat GDT and set CS/DS/ES/SS before jumping to the entry point.
It writes a banner to the VGA text buffer at 0xB8000, echoes it to COM1
(0x3F8), then signals the debug-exit port (0xF4).
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "kernels" / "HELLO.BIN"

KERNEL_BASE = 0x00100000
MESSAGE_OFFSET = 0x200
MESSAGE_VA = KERNEL_BASE + MESSAGE_OFFSET
VGA_VA = 0x000B8000
SERIAL_PORT = 0x3F8
EXIT_PORT = 0xF4
ATTRIBUTE = 0x1F  # white on blue

MESSAGE = b"Hello from the OTW minimal kernel!"


def u32(value: int) -> bytes:
    return struct.pack("<I", value & 0xFFFFFFFF)


def build_code() -> bytes:
    code = bytearray()
    code += b"\xfa"                      # cli
    code += b"\xbc" + u32(0x00200000)    # mov esp, 0x00200000
    code += b"\xfc"                      # cld

    # Copy MESSAGE to VGA text memory, interleaving a color attribute.
    code += b"\xbe" + u32(MESSAGE_VA)    # mov esi, message
    code += b"\xbf" + u32(VGA_VA)        # mov edi, vga
    code += b"\xb9" + u32(len(MESSAGE))  # mov ecx, len
    code += b"\xac"                      # lodsb
    code += b"\xaa"                      # stosb
    code += b"\xc6\x07" + bytes([ATTRIBUTE])  # mov byte [edi], attr
    code += b"\x47"                      # inc edi
    code += b"\xe2\xf8"                  # loop back 8 bytes

    # Echo the same message to the emulated serial port.
    code += b"\xbe" + u32(MESSAGE_VA)    # mov esi, message
    code += b"\xb9" + u32(len(MESSAGE))  # mov ecx, len
    code += b"\x66\xba" + struct.pack("<H", SERIAL_PORT)  # mov dx, 0x3f8
    code += b"\xac"                      # lodsb
    code += b"\xee"                      # out dx, al
    code += b"\xe2\xfc"                  # loop back 4 bytes

    # Tell the host to stop with exit status 0.
    code += b"\x66\xba" + struct.pack("<H", EXIT_PORT)
    code += b"\xb0\x00"                  # mov al, 0
    code += b"\xee"                      # out dx, al
    code += b"\xf4"                      # hlt
    return bytes(code)


def build_image() -> bytes:
    code = build_code()
    if len(code) > MESSAGE_OFFSET:
        raise SystemExit(f"kernel code is {len(code)} bytes; limit is {MESSAGE_OFFSET}")

    image = bytearray(MESSAGE_OFFSET)
    image[: len(code)] = code
    image[MESSAGE_OFFSET:MESSAGE_OFFSET + len(MESSAGE)] = MESSAGE
    return bytes(image)


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    image = build_image()
    OUTPUT.write_bytes(image)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(image)} bytes)")


if __name__ == "__main__":
    main()
