#!/usr/bin/env python3
"""Build a 512-byte real-mode boot sector for the machine layer.

It uses BIOS INT 10h/AH=0x0E (teletype) to print a banner, then writes 0 to
the debug-exit port 0xF4. The final two bytes are the 0x55AA boot signature.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "boot" / "OTWBOOT.BIN"

BOOT_LOADED_AT = 0x7C00
MESSAGE = b"OTW BIOS boot OK\r\n"


def u16(value: int) -> bytes:
    return struct.pack("<H", value & 0xFFFF)


def build_boot_sector() -> bytes:
    code = bytearray()
    code += b"\xfc"                      # cld
    code += b"\xbe" + u16(0)             # mov si, message (patched below)
    msg_operand = 2

    code += b"\xb4\x0e"                  # mov ah, 0x0e (teletype)
    loop_offset = len(code)
    code += b"\xac"                      # lodsb
    code += b"\x3c\x00"                  # cmp al, 0
    je_offset = len(code)
    code += b"\x74\x00"                  # je done (patched below)
    code += b"\xcd\x10"                  # int 0x10
    jmp_offset = len(code)
    code += b"\xeb\x00"                  # jmp loop (patched below)
    done_offset = len(code)

    code += b"\xba\xf4\x00"              # mov dx, 0xf4
    code += b"\xb0\x00"                  # mov al, 0
    code += b"\xee"                      # out dx, al
    code += b"\xf4"                      # hlt

    message_image_offset = len(code)
    # DS is 0 at boot, so SI must be the linear address of the message.
    message_linear = BOOT_LOADED_AT + message_image_offset
    code[msg_operand:msg_operand + 2] = u16(message_linear)
    code[je_offset + 1] = (done_offset - (je_offset + 2)) & 0xFF
    code[jmp_offset + 1] = (loop_offset - (jmp_offset + 2)) & 0xFF

    image = bytearray(512)
    image[: len(code)] = code
    image[message_image_offset:message_image_offset + len(MESSAGE)] = MESSAGE
    image[510:512] = b"\x55\xaa"
    return bytes(image)


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    image = build_boot_sector()
    OUTPUT.write_bytes(image)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(image)} bytes)")


if __name__ == "__main__":
    main()
