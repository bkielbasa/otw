#!/usr/bin/env python3
"""Build HELLO64.EXE, a tiny 64-bit (PE32+) guest for the x64 tests.

No cross-compiler is required: the x86-64 machine code, headers, import
table and data are emitted directly. Like a linker's default it is based at
0x140000000, above 4 GiB, with base relocations, so the VM must move it.

It writes a line to standard output (GetStdHandle, WriteFile), shows a
message box (MessageBoxW), and exits with the message box's result plus 40.
The message's address is stored as an absolute 64-bit pointer in the data,
fixed up by a DIR64 relocation, to exercise rebasing.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "HELLO64.EXE"

IMAGE_BASE = 0x1_4000_0000
PE_OFFSET = 0x80
SECTION_RVA = 0x1000
SECTION_RAW = 0x200
SECTION_SIZE = 0x600
RELOC_RVA = 0x2000
RELOC_RAW = SECTION_RAW + SECTION_SIZE
SIZE_OF_IMAGE = 0x3000
SIZE_OF_HEADERS = 0x200

# Offsets in .text.
CODE = 0x000
MESSAGE = 0x200
MESSAGE_PTR = 0x240
WRITTEN = 0x248
TEXT = 0x260
CAPTION = 0x2C0
IMPORTS = 0x300
NAMES = 0x380

MESSAGE_BYTES = b"Hello from a 64-bit program on OTW!\r\n"
TEXT_W = "64-bit code is running.\0".encode("utf-16-le")
CAPTION_W = "HELLO64\0".encode("utf-16-le")

DLLS = [
    ("KERNEL32.dll", ["GetStdHandle", "WriteFile", "ExitProcess"]),
    ("USER32.dll", ["MessageBoxW"]),
]


def rva(offset: int) -> int:
    return SECTION_RVA + offset


def build_imports(section: bytearray) -> dict[str, int]:
    """Lay out descriptors, lookup tables, IATs and names; return IAT slot offsets."""
    descriptors = IMPORTS
    cursor = NAMES
    slots: dict[str, int] = {}
    tables = []
    for dll, names in DLLS:
        hint_rvas = []
        for name in names:
            hint_rvas.append(rva(cursor))
            blob = struct.pack("<H", 0) + name.encode() + b"\0"
            blob += b"\0" * (len(blob) % 2)
            section[cursor:cursor + len(blob)] = blob
            cursor += len(blob)
        dll_name = rva(cursor)
        section[cursor:cursor + len(dll) + 1] = dll.encode() + b"\0"
        cursor += len(dll) + 1
        tables.append((dll_name, names, hint_rvas))
    cursor = (cursor + 7) & ~7
    for index, (dll_name, names, hint_rvas) in enumerate(tables):
        lookup = cursor
        cursor += 8 * (len(names) + 1)
        iat = cursor
        cursor += 8 * (len(names) + 1)
        for slot, (name, hint) in enumerate(zip(names, hint_rvas)):
            section[lookup + slot * 8:lookup + slot * 8 + 8] = struct.pack("<Q", hint)
            section[iat + slot * 8:iat + slot * 8 + 8] = struct.pack("<Q", hint)
            slots[name] = iat + slot * 8
        at = descriptors + index * 20
        section[at:at + 20] = struct.pack("<IIIII", rva(lookup), 0, 0, dll_name, rva(iat))
    assert cursor <= SECTION_SIZE
    return slots


def build_code(slots: dict[str, int]) -> bytes:
    code = bytearray()

    def rip(target: int, length_after: int = 0) -> bytes:
        # disp32 relative to the end of the instruction (4 bytes of disp plus
        # any immediate that follows it).
        return struct.pack("<i", rva(target) - rva(CODE + len(code) + 4 + length_after))

    def call(name: str) -> None:
        code.extend(b"\xff\x15")
        code.extend(rip(slots[name]))

    code += b"\x48\x83\xec\x38"                      # sub rsp, 0x38
    code += b"\xb9\xf5\xff\xff\xff"                  # mov ecx, -11
    call("GetStdHandle")
    code += b"\x48\x89\xc1"                          # mov rcx, rax
    code += b"\x48\x8b\x15"; code += rip(MESSAGE_PTR)  # mov rdx, [rip+message_ptr]
    code += b"\x41\xb8" + struct.pack("<I", len(MESSAGE_BYTES))  # mov r8d, len
    code += b"\x4c\x8d\x0d"; code += rip(WRITTEN)    # lea r9, [rip+written]
    code += b"\x48\xc7\x44\x24\x20\x00\x00\x00\x00"  # mov qword [rsp+0x20], 0
    call("WriteFile")
    code += b"\x31\xc9"                              # xor ecx, ecx
    code += b"\x48\x8d\x15"; code += rip(TEXT)       # lea rdx, [rip+text]
    code += b"\x4c\x8d\x05"; code += rip(CAPTION)    # lea r8, [rip+caption]
    code += b"\x41\xb9\x00\x00\x00\x00"              # mov r9d, MB_OK
    call("MessageBoxW")
    code += b"\x8d\x48\x28"                          # lea ecx, [rax+40]
    call("ExitProcess")
    code += b"\xcc"                                  # int3, unreachable
    return bytes(code)


def build() -> bytes:
    section = bytearray(SECTION_SIZE)
    slots = build_imports(section)
    code = build_code(slots)
    section[CODE:CODE + len(code)] = code
    section[MESSAGE:MESSAGE + len(MESSAGE_BYTES)] = MESSAGE_BYTES
    section[MESSAGE_PTR:MESSAGE_PTR + 8] = struct.pack("<Q", IMAGE_BASE + rva(MESSAGE))
    section[TEXT:TEXT + len(TEXT_W)] = TEXT_W
    section[CAPTION:CAPTION + len(CAPTION_W)] = CAPTION_W

    # One relocation block: the DIR64 fixup of the message pointer.
    page = rva(MESSAGE_PTR) & ~0xFFF
    entries = [(10 << 12) | (rva(MESSAGE_PTR) & 0xFFF), 0]
    reloc = struct.pack("<II", page, 8 + 2 * len(entries)) + b"".join(struct.pack("<H", e) for e in entries)
    reloc_raw = reloc + b"\0" * (0x200 - len(reloc))

    headers = bytearray(SIZE_OF_HEADERS)
    headers[0:2] = b"MZ"
    headers[0x3C:0x40] = struct.pack("<I", PE_OFFSET)
    at = PE_OFFSET
    headers[at:at + 4] = b"PE\0\0"
    # COFF: AMD64, two sections, PE32+ optional header, executable, large address aware.
    headers[at + 4:at + 24] = struct.pack("<HHIIIHH", 0x8664, 2, 0, 0, 0, 0xF0, 0x0022)
    o = at + 24
    optional = struct.pack(
        "<HBBIIIII",
        0x20B, 14, 0, SECTION_SIZE, 0x200, 0, rva(CODE), SECTION_RVA,
    )
    optional += struct.pack("<QII", IMAGE_BASE, 0x1000, 0x200)
    optional += struct.pack("<HHHHHHI", 6, 0, 0, 0, 6, 0, 0)
    optional += struct.pack("<IIIHH", SIZE_OF_IMAGE, SIZE_OF_HEADERS, 0, 3, 0x8160)
    optional += struct.pack("<QQQQII", 0x100000, 0x1000, 0x100000, 0x1000, 0, 16)
    directories = [(0, 0)] * 16
    directories[1] = (rva(IMPORTS), 40 + 20)
    directories[5] = (RELOC_RVA, len(reloc))
    optional += b"".join(struct.pack("<II", *d) for d in directories)
    assert len(optional) == 0xF0
    headers[o:o + 0xF0] = optional
    sections = o + 0xF0
    headers[sections:sections + 40] = b".text\0\0\0" + struct.pack(
        "<IIIIIIHHI", SECTION_SIZE, SECTION_RVA, SECTION_SIZE, SECTION_RAW, 0, 0, 0, 0, 0xE0000060)
    headers[sections + 40:sections + 80] = b".reloc\0\0" + struct.pack(
        "<IIIIIIHHI", len(reloc), RELOC_RVA, 0x200, RELOC_RAW, 0, 0, 0, 0, 0x42000040)
    return bytes(headers) + bytes(section) + reloc_raw


def main() -> None:
    OUTPUT.write_bytes(build())
    print(f"wrote {OUTPUT}")


if __name__ == "__main__":
    main()
