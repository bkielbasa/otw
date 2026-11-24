#!/usr/bin/env python3
"""Build the tiny Winsock smoke-test PE used by the VM integration tests.

No cross-compiler is required: the x86 machine code, PE headers, import table,
and guest data are emitted directly. The guest imports WSOCK32.dll and
KERNEL32.dll, connects to a host-supplied 127.0.0.1 port, sends ``ping``, reads
the echo, and exits with 0 only when the round trip succeeds.

The integration test patches the two port bytes at SOCKADDR_OFF + 2 before
running, so the binary itself has no dependency on a fixed port.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "WINSOCK.EXE"

IMAGE_BASE = 0x00400000
PE_OFFSET = 0x80
SECTION_RVA = 0x1000
SECTION_RAW_OFFSET = 0x200
SECTION_RAW_SIZE = 0x800
SIZE_OF_IMAGE = 0x2000
SIZE_OF_HEADERS = 0x200

# Offsets relative to the .text section. Keep the sockaddr layout in sync with
# tests/winsock_vm.rs.
CODE_OFF = 0x000
WSADATA_OFF = 0x180
SOCKADDR_OFF = 0x390
MESSAGE_OFF = 0x3A0
BUFFER_OFF = 0x3B0
SOCKET_OFF = 0x3C0
IMPORT_DESC_OFF = 0x400
DLL_NAME_OFF = 0x440
HINT_NAMES_OFF = 0x480
WINSOCK_INT_OFF = 0x500
KERNEL_INT_OFF = 0x520
WINSOCK_IAT_OFF = 0x540
KERNEL_IAT_OFF = 0x560

WINSOCK_IMPORTS = [
    "WSAStartup",
    "socket",
    "connect",
    "send",
    "recv",
    "closesocket",
    "WSACleanup",
]
KERNEL_IMPORTS = ["ExitProcess"]
ALL_IMPORTS = WINSOCK_IMPORTS + KERNEL_IMPORTS

MESSAGE = b"ping\0"


def rva(section_offset: int) -> int:
    return SECTION_RVA + section_offset


def va(section_offset: int) -> int:
    return IMAGE_BASE + rva(section_offset)


def u32(value: int) -> bytes:
    return struct.pack("<I", value & 0xFFFFFFFF)


def iat_va(name: str) -> int:
    if name in WINSOCK_IMPORTS:
        return va(WINSOCK_IAT_OFF + WINSOCK_IMPORTS.index(name) * 4)
    return va(KERNEL_IAT_OFF + KERNEL_IMPORTS.index(name) * 4)


class Code:
    """Tiny x86 emitter with rel32 jump fixups to a named label."""

    def __init__(self) -> None:
        self.bytes = bytearray()
        self.labels: dict[str, int] = {}
        self.fixups: list[tuple[int, str]] = []

    def emit(self, *chunks: bytes) -> None:
        for chunk in chunks:
            self.bytes.extend(chunk)

    def mark(self, label: str) -> None:
        self.labels[label] = len(self.bytes)

    def push_imm(self, value: int) -> None:
        if -128 <= value <= 127:
            self.emit(b"\x6a", bytes([value & 0xFF]))
        else:
            self.emit(b"\x68", u32(value))

    def push_dword_ptr(self, address: int) -> None:
        # FF /6: push dword ptr [disp32]
        self.emit(b"\xff\x35", u32(address))

    def call_iat(self, name: str) -> None:
        # FF /2: call dword ptr [disp32]
        self.emit(b"\xff\x15", u32(iat_va(name)))

    def store_eax(self, address: int) -> None:
        self.emit(b"\xa3", u32(address))

    def jcc(self, opcode: int, label: str) -> None:
        # 0F 84/85 rel32: JE/JNE with a 32-bit displacement.
        fixup = len(self.bytes) + 2
        self.emit(b"\x0f", bytes([opcode]), b"\0\0\0\0")
        self.fixups.append((fixup, label))

    def finish(self) -> bytes:
        for offset, label in self.fixups:
            target = self.labels[label]
            displacement = target - (offset + 4)
            self.bytes[offset : offset + 4] = u32(displacement)
        return bytes(self.bytes)


def build_code() -> bytes:
    code = Code()

    # WSAStartup(0x0101, &wsadata)
    code.push_dword_ptr(va(WSADATA_OFF))
    code.push_imm(0x0101)
    code.call_iat("WSAStartup")
    code.emit(b"\x85\xc0")  # test eax, eax
    code.jcc(0x85, "fail")  # jnz

    # socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    code.push_imm(6)
    code.push_imm(1)
    code.push_imm(2)
    code.call_iat("socket")
    code.emit(b"\x3d", u32(0xFFFFFFFF))  # cmp eax, INVALID_SOCKET
    code.jcc(0x84, "fail")  # je
    code.store_eax(va(SOCKET_OFF))

    # connect(socket, &sockaddr_in, 16)
    code.push_imm(16)
    code.push_imm(va(SOCKADDR_OFF))
    code.push_dword_ptr(va(SOCKET_OFF))
    code.call_iat("connect")
    code.emit(b"\x85\xc0")
    code.jcc(0x85, "fail")

    # send(socket, "ping", 4, 0)
    code.push_imm(0)
    code.push_imm(4)
    code.push_imm(va(MESSAGE_OFF))
    code.push_dword_ptr(va(SOCKET_OFF))
    code.call_iat("send")
    code.emit(b"\x3d", u32(4))
    code.jcc(0x85, "fail")

    # recv(socket, buffer, 16, 0)
    code.push_imm(0)
    code.push_imm(16)
    code.push_imm(va(BUFFER_OFF))
    code.push_dword_ptr(va(SOCKET_OFF))
    code.call_iat("recv")
    code.emit(b"\x3d", u32(4))
    code.jcc(0x85, "fail")

    # Verify the echo byte by byte.
    for index, byte in enumerate(b"ping"):
        code.emit(b"\x80\x3d", u32(va(BUFFER_OFF + index)), bytes([byte]))
        code.jcc(0x85, "fail")

    # Success: closesocket(socket); WSACleanup(); ExitProcess(0).
    code.push_dword_ptr(va(SOCKET_OFF))
    code.call_iat("closesocket")
    code.call_iat("WSACleanup")
    code.push_imm(0)
    code.call_iat("ExitProcess")
    code.emit(b"\xf4")  # hlt, unreachable

    code.mark("fail")
    code.call_iat("WSACleanup")
    code.push_imm(1)
    code.call_iat("ExitProcess")
    code.emit(b"\xf4")
    return code.finish()


def build_hint_names() -> tuple[bytes, dict[str, int]]:
    blob = bytearray()
    entries: dict[str, int] = {}
    for index, name in enumerate(ALL_IMPORTS):
        entries[name] = rva(HINT_NAMES_OFF + len(blob))
        blob += struct.pack("<H", index)
        blob += name.encode("ascii") + b"\0"
        while len(blob) % 2:
            blob += b"\0"
    return bytes(blob), entries


def build_section() -> bytes:
    section = bytearray(SECTION_RAW_SIZE)
    code = build_code()
    section[CODE_OFF : CODE_OFF + len(code)] = code

    section[WSADATA_OFF : WSADATA_OFF + 528] = b"\0" * 528
    sockaddr = bytearray(16)
    sockaddr[0:2] = struct.pack("<H", 2)  # AF_INET
    sockaddr[2:4] = b"\x30\x39"  # patched by the integration test
    sockaddr[4:8] = bytes([127, 0, 0, 1])
    section[SOCKADDR_OFF : SOCKADDR_OFF + 16] = sockaddr
    section[MESSAGE_OFF : MESSAGE_OFF + len(MESSAGE)] = MESSAGE

    hint_names, hint_name_rvas = build_hint_names()
    section[HINT_NAMES_OFF : HINT_NAMES_OFF + len(hint_names)] = hint_names

    def descriptor(int_rva: int, name_rva: int, iat_rva: int) -> bytes:
        return struct.pack("<IIIII", int_rva, 0, 0, name_rva, iat_rva)

    descriptors = descriptor(
        rva(WINSOCK_INT_OFF), rva(DLL_NAME_OFF), rva(WINSOCK_IAT_OFF)
    )
    descriptors += descriptor(
        rva(KERNEL_INT_OFF),
        rva(DLL_NAME_OFF + len(b"WSOCK32.dll\0")),
        rva(KERNEL_IAT_OFF),
    )
    descriptors += b"\0" * 20
    section[IMPORT_DESC_OFF : IMPORT_DESC_OFF + len(descriptors)] = descriptors

    section[DLL_NAME_OFF : DLL_NAME_OFF + len(b"WSOCK32.dll\0")] = b"WSOCK32.dll\0"
    kernel_name_off = DLL_NAME_OFF + len(b"WSOCK32.dll\0")
    section[kernel_name_off : kernel_name_off + len(b"KERNEL32.dll\0")] = b"KERNEL32.dll\0"

    for group, int_off, iat_off in (
        (WINSOCK_IMPORTS, WINSOCK_INT_OFF, WINSOCK_IAT_OFF),
        (KERNEL_IMPORTS, KERNEL_INT_OFF, KERNEL_IAT_OFF),
    ):
        for index, name in enumerate(group):
            hint_rva = hint_name_rvas[name]
            section[int_off + index * 4 : int_off + index * 4 + 4] = u32(hint_rva)
            section[iat_off + index * 4 : iat_off + index * 4 + 4] = u32(hint_rva)

    return bytes(section)


def build_optional_header() -> bytes:
    header = bytearray(0xE0)
    struct.pack_into("<H", header, 0, 0x010B)  # PE32
    struct.pack_into("<I", header, 16, rva(CODE_OFF))  # AddressOfEntryPoint
    struct.pack_into("<I", header, 20, rva(CODE_OFF))  # BaseOfCode
    struct.pack_into("<I", header, 24, rva(WSADATA_OFF))  # BaseOfData
    struct.pack_into("<I", header, 28, IMAGE_BASE)
    struct.pack_into("<I", header, 32, 0x1000)  # SectionAlignment
    struct.pack_into("<I", header, 36, 0x200)  # FileAlignment
    struct.pack_into("<H", header, 40, 4)  # OS major
    struct.pack_into("<H", header, 48, 4)  # Image major
    struct.pack_into("<I", header, 56, SIZE_OF_IMAGE)
    struct.pack_into("<I", header, 60, SIZE_OF_HEADERS)
    struct.pack_into("<H", header, 68, 3)  # console subsystem
    struct.pack_into("<I", header, 72, 0x00100000)  # stack reserve
    struct.pack_into("<I", header, 76, 0x00001000)  # stack commit
    struct.pack_into("<I", header, 80, 0x00100000)  # heap reserve
    struct.pack_into("<I", header, 84, 0x00001000)  # heap commit
    struct.pack_into("<I", header, 92, 16)  # data directory count
    struct.pack_into(
        "<II", header, 96 + 8, rva(IMPORT_DESC_OFF), 3 * 20
    )  # import directory
    return bytes(header)


def build_pe() -> bytes:
    dos = bytearray(PE_OFFSET)
    dos[0:2] = b"MZ"
    struct.pack_into("<I", dos, 0x3C, PE_OFFSET)

    coff = struct.pack(
        "<HHIIIHH",
        0x014C,  # i386
        1,  # one section
        0,  # timestamp
        0,  # symbol table
        0,  # symbol count
        0x00E0,  # optional header size
        0x0102,  # executable + 32-bit machine
    )

    section_header = struct.pack(
        "<8sIIIIIIHHI",
        b".text\0\0\0",
        SECTION_RAW_SIZE,
        SECTION_RVA,
        SECTION_RAW_SIZE,
        SECTION_RAW_OFFSET,
        0,
        0,
        0,
        0,
        0x60000020,  # code + execute + read
    )

    headers = bytearray(SIZE_OF_HEADERS)
    headers[: len(dos)] = dos
    offset = PE_OFFSET
    headers[offset : offset + 4] = b"PE\0\0"
    offset += 4
    headers[offset : offset + len(coff)] = coff
    offset += len(coff)
    optional = build_optional_header()
    headers[offset : offset + len(optional)] = optional
    offset += len(optional)
    headers[offset : offset + len(section_header)] = section_header

    return bytes(headers) + build_section()


def main() -> None:
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    pe = build_pe()
    OUTPUT.write_bytes(pe)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(pe)} bytes)")


if __name__ == "__main__":
    main()
