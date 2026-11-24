#!/usr/bin/env python3
"""Build the tiny Win32 Hello World PE used as the VM's first execution test.

No cross-compiler is required: the x86 machine code, PE headers, import table,
and guest data are emitted directly. The resulting image imports only three
KERNEL32 functions: GetStdHandle, WriteFile, and ExitProcess.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "HELLO.EXE"

IMAGE_BASE = 0x00400000
PE_OFFSET = 0x80
SECTION_RVA = 0x1000
SECTION_RAW_OFFSET = 0x200
SECTION_RAW_SIZE = 0x400
SIZE_OF_IMAGE = 0x2000
SIZE_OF_HEADERS = 0x200

# Offsets relative to the .text section.
CODE_OFF = 0x000
MESSAGE_OFF = 0x100
IMPORT_DESC_OFF = 0x200
INT_OFF = 0x240
DLL_NAME_OFF = 0x250
HINT_NAMES_OFF = 0x260
IAT_OFF = 0x2A0
BYTES_WRITTEN_OFF = 0x300

MESSAGE = b"Hello from the OTW Win95 VM!\r\n\0"
MESSAGE_LEN = len(MESSAGE) - 1

IMPORT_NAMES = ["GetStdHandle", "WriteFile", "ExitProcess"]


def rva(section_offset: int) -> int:
    return SECTION_RVA + section_offset


def va(section_offset: int) -> int:
    return IMAGE_BASE + rva(section_offset)


def u32(value: int) -> bytes:
    return struct.pack("<I", value & 0xFFFFFFFF)


def build_code() -> bytes:
    stdout_iat = va(IAT_OFF)
    write_file_iat = va(IAT_OFF + 4)
    exit_process_iat = va(IAT_OFF + 8)
    message_va = va(MESSAGE_OFF)
    bytes_written_va = va(BYTES_WRITTEN_OFF)

    code = bytearray()
    # GetStdHandle(STD_OUTPUT_HANDLE)
    code += b"\x6a\xf5"                 # push -11
    code += b"\xff\x15" + u32(stdout_iat)  # call dword ptr [GetStdHandle]

    # WriteFile(handle, message, len, &bytes_written, NULL)
    code += b"\x6a\x00"                 # push NULL
    code += b"\x68" + u32(bytes_written_va)
    code += b"\x68" + u32(MESSAGE_LEN)
    code += b"\x68" + u32(message_va)
    code += b"\x50"                     # push eax (stdout handle)
    code += b"\xff\x15" + u32(write_file_iat)

    # ExitProcess(0)
    code += b"\x6a\x00"
    code += b"\xff\x15" + u32(exit_process_iat)
    code += b"\xf4"                     # hlt, unreachable
    return bytes(code)


def build_hint_names() -> tuple[bytes, list[int]]:
    blob = bytearray()
    entries: list[int] = []
    for name in IMPORT_NAMES:
        entries.append(rva(HINT_NAMES_OFF + len(blob)))
        blob += struct.pack("<H", len(entries))
        blob += name.encode("ascii") + b"\0"
        while len(blob) % 2:
            blob += b"\0"
    return bytes(blob), entries


def build_section() -> bytes:
    section = bytearray(SECTION_RAW_SIZE)
    code = build_code()
    section[CODE_OFF:CODE_OFF + len(code)] = code
    section[MESSAGE_OFF:MESSAGE_OFF + len(MESSAGE)] = MESSAGE

    hint_names, hint_name_rvas = build_hint_names()
    section[HINT_NAMES_OFF:HINT_NAMES_OFF + len(hint_names)] = hint_names

    # IMAGE_IMPORT_DESCRIPTOR for KERNEL32.dll, followed by a null descriptor.
    descriptor = struct.pack(
        "<IIIII",
        rva(INT_OFF),
        0,
        0,
        rva(DLL_NAME_OFF),
        rva(IAT_OFF),
    )
    section[IMPORT_DESC_OFF:IMPORT_DESC_OFF + 20] = descriptor
    section[IMPORT_DESC_OFF + 20:IMPORT_DESC_OFF + 40] = b"\0" * 20

    section[DLL_NAME_OFF:DLL_NAME_OFF + len(b"KERNEL32.dll\0")] = b"KERNEL32.dll\0"

    for index, hint_rva in enumerate(hint_name_rvas):
        offset = INT_OFF + index * 4
        section[offset:offset + 4] = u32(hint_rva)
        section[IAT_OFF + index * 4:IAT_OFF + index * 4 + 4] = u32(hint_rva)

    return bytes(section)


def build_optional_header() -> bytes:
    header = bytearray(0xE0)
    struct.pack_into("<H", header, 0, 0x010B)              # PE32
    struct.pack_into("<I", header, 16, rva(CODE_OFF))      # AddressOfEntryPoint
    struct.pack_into("<I", header, 20, rva(CODE_OFF))      # BaseOfCode
    struct.pack_into("<I", header, 24, rva(MESSAGE_OFF))   # BaseOfData
    struct.pack_into("<I", header, 28, IMAGE_BASE)
    struct.pack_into("<I", header, 32, 0x1000)             # SectionAlignment
    struct.pack_into("<I", header, 36, 0x200)              # FileAlignment
    struct.pack_into("<H", header, 40, 4)                  # OS major
    struct.pack_into("<H", header, 48, 4)                  # Image major
    struct.pack_into("<I", header, 56, SIZE_OF_IMAGE)
    struct.pack_into("<I", header, 60, SIZE_OF_HEADERS)
    struct.pack_into("<H", header, 68, 3)                  # console subsystem
    struct.pack_into("<I", header, 72, 0x00100000)         # stack reserve
    struct.pack_into("<I", header, 76, 0x00001000)         # stack commit
    struct.pack_into("<I", header, 80, 0x00100000)         # heap reserve
    struct.pack_into("<I", header, 84, 0x00001000)         # heap commit
    struct.pack_into("<I", header, 92, 16)                 # data directory count
    struct.pack_into("<II", header, 96 + 8, rva(IMPORT_DESC_OFF), 40)
    return bytes(header)


def build_pe() -> bytes:
    dos = bytearray(PE_OFFSET)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, PE_OFFSET)

    coff = struct.pack(
        "<HHIIIHH",
        0x014C,          # i386
        1,               # one section
        0,               # timestamp
        0,               # symbol table
        0,               # symbol count
        0x00E0,          # optional header size
        0x0102,          # executable + 32-bit machine
    )

    section_header = struct.pack(
        "<8sIIIIIIHHI",
        b".text\0\0\0",
        SECTION_RAW_SIZE,
        SECTION_RVA,
        SECTION_RAW_SIZE,
        SECTION_RAW_OFFSET,
        0, 0, 0, 0,
        0x60000020,      # code + execute + read
    )

    headers = bytearray(SIZE_OF_HEADERS)
    headers[: len(dos)] = dos
    offset = PE_OFFSET
    headers[offset:offset + 4] = b"PE\0\0"
    offset += 4
    headers[offset:offset + len(coff)] = coff
    offset += len(coff)
    optional = build_optional_header()
    headers[offset:offset + len(optional)] = optional
    offset += len(optional)
    headers[offset:offset + len(section_header)] = section_header

    return bytes(headers) + build_section()


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    pe = build_pe()
    OUTPUT.write_bytes(pe)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(pe)} bytes)")


if __name__ == "__main__":
    main()
