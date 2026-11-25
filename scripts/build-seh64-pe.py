#!/usr/bin/env python3
"""Build SEH64.EXE, a tiny 64-bit program for x64 exception handling tests.

No cross-compiler is required: machine code, unwind information
(`.pdata`/`.xdata`), imports and headers are emitted directly.

`main` has a language handler. It raises a software exception
(RaiseException 0xE0000001), then calls `fault`, which reads address 0 (an
access violation) inside a frame of its own (push rbx; sub rsp, 0x20).
For each, the dispatcher walks to `main`'s frame and calls the handler,
which unwinds to a continuation in `main` with RtlUnwindEx, returning
0x1000 for the software exception and 0x234 for the fault. `main` exits
with their sum, 0x1234: both exceptions were dispatched through the
function tables and both unwinds restored `main`'s frame (the sum is kept
in a nonvolatile register across the second one).
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "SEH64.EXE"

IMAGE_BASE = 0x40_0000
PE_OFFSET = 0x80
SECTION_RVA = 0x1000
SECTION_RAW = 0x200
SECTION_SIZE = 0x800
SIZE_OF_IMAGE = 0x2000
SIZE_OF_HEADERS = 0x200

MAIN = 0x000
FAULT = 0x100
HANDLER = 0x140
XDATA = 0x300
PDATA = 0x380
IMPORTS = 0x400
NAMES = 0x480

FUNCTIONS = ["RaiseException", "ExitProcess", "RtlUnwindEx"]


def rva(offset: int) -> int:
    return SECTION_RVA + offset


class Code:
    def __init__(self, base: int):
        self.base = base
        self.bytes = bytearray()
        self.fixups = []  # (position of disp32, target offset, bytes after disp)
        self.labels = {}

    def here(self) -> int:
        return self.base + len(self.bytes)

    def label(self, name: str) -> None:
        self.labels[name] = self.here()

    def emit(self, data: bytes) -> None:
        self.bytes += data

    def rip(self, prefix: bytes, target, after: int = 0) -> None:
        """An instruction with a RIP-relative disp32 to `target` (an offset or a label)."""
        self.bytes += prefix
        self.fixups.append((len(self.bytes), target, after))
        self.bytes += b"\0\0\0\0"

    def resolve(self) -> bytes:
        for position, target, after in self.fixups:
            target = self.labels[target] if isinstance(target, str) else target
            end = self.base + position + 4 + after
            self.bytes[position:position + 4] = struct.pack("<i", target - end)
        return bytes(self.bytes)


def build_imports(section: bytearray) -> dict:
    cursor = NAMES
    hints = []
    for name in FUNCTIONS:
        hints.append(rva(cursor))
        blob = struct.pack("<H", 0) + name.encode() + b"\0"
        blob += b"\0" * (len(blob) % 2)
        section[cursor:cursor + len(blob)] = blob
        cursor += len(blob)
    dll_name = rva(cursor)
    section[cursor:cursor + 13] = b"KERNEL32.dll\0"
    cursor = (cursor + 13 + 7) & ~7
    lookup = cursor
    iat = lookup + 8 * (len(FUNCTIONS) + 1)
    slots = {}
    for index, (name, hint) in enumerate(zip(FUNCTIONS, hints)):
        struct.pack_into("<Q", section, lookup + index * 8, hint)
        struct.pack_into("<Q", section, iat + index * 8, hint)
        slots[name] = iat + index * 8
    assert iat + 8 * (len(FUNCTIONS) + 1) <= SECTION_SIZE
    struct.pack_into("<IIIII", section, IMPORTS, rva(lookup), 0, 0, dll_name, rva(iat))
    return slots


def build_main(slots: dict) -> Code:
    c = Code(MAIN)
    c.emit(b"\x48\x83\xec\x28")                  # sub rsp, 0x28 (prolog, 4 bytes)
    c.emit(b"\xb9\x01\x00\x00\xe0")              # mov ecx, 0xE0000001
    c.emit(b"\x31\xd2")                          # xor edx, edx
    c.emit(b"\x45\x31\xc0")                      # xor r8d, r8d
    c.emit(b"\x45\x31\xc9")                      # xor r9d, r9d
    c.rip(b"\xff\x15", slots["RaiseException"])  # call [RaiseException]
    c.emit(b"\x90")
    c.emit(b"\xb9\x01\x00\x00\x00")              # mov ecx, 1 (not reached)
    c.rip(b"\xff\x15", slots["ExitProcess"])
    c.label("target1")                           # eax = 0x1000
    c.emit(b"\x89\xc3")                          # mov ebx, eax
    c.rip(b"\xe8", FAULT)                        # call fault
    c.emit(b"\x90")
    c.emit(b"\xb9\x02\x00\x00\x00")              # mov ecx, 2 (not reached)
    c.rip(b"\xff\x15", slots["ExitProcess"])
    c.label("target2")                           # eax = 0x234
    c.emit(b"\x01\xd8")                          # add eax, ebx
    c.emit(b"\x89\xc1")                          # mov ecx, eax
    c.rip(b"\xff\x15", slots["ExitProcess"])
    c.emit(b"\xcc")
    c.label("end")
    return c


def build_fault() -> Code:
    c = Code(FAULT)
    c.emit(b"\x53")                              # push rbx
    c.emit(b"\x48\x83\xec\x20")                  # sub rsp, 0x20 (prolog ends at 5)
    c.emit(b"\xbb\xef\xbe\xad\xde")              # mov ebx, 0xDEADBEEF (restored by unwinding)
    c.emit(b"\x31\xc0")                          # xor eax, eax
    c.emit(b"\x8b\x00")                          # mov eax, [rax]: access violation
    c.emit(b"\x48\x83\xc4\x20")                  # add rsp, 0x20
    c.emit(b"\x5b")                              # pop rbx
    c.emit(b"\xc3")                              # ret
    c.label("end")
    return c


def build_handler(slots: dict, main: Code) -> Code:
    c = Code(HANDLER)
    c.labels.update(main.labels)
    c.emit(b"\x48\x83\xec\x38")                  # sub rsp, 0x38 (prolog, 4 bytes)
    c.emit(b"\x8b\x01")                          # mov eax, [rcx] (exception code)
    c.rip(b"\x4c\x8d\x15", "target1")            # lea r10, [rip+target1]
    c.emit(b"\x41\xbb\x00\x10\x00\x00")          # mov r11d, 0x1000
    c.emit(b"\x3d\x05\x00\x00\xc0")              # cmp eax, 0xC0000005
    c.emit(b"\x75\x0d")                          # jne +13
    c.rip(b"\x4c\x8d\x15", "target2")            # lea r10, [rip+target2] (7 bytes)
    c.emit(b"\x41\xbb\x34\x02\x00\x00")          # mov r11d, 0x234 (6 bytes)
    # RtlUnwindEx(frame, target, record, value, context, history)
    c.emit(b"\x48\x89\xc8")                      # mov rax, rcx (record)
    c.emit(b"\x48\x89\xd1")                      # mov rcx, rdx (establisher frame)
    c.emit(b"\x4c\x89\xd2")                      # mov rdx, r10 (target)
    c.emit(b"\x4c\x89\x44\x24\x20")              # mov [rsp+0x20], r8 (context)
    c.emit(b"\x49\x89\xc0")                      # mov r8, rax (record)
    c.emit(b"\x4d\x89\xd9")                      # mov r9, r11 (value)
    c.emit(b"\x48\xc7\x44\x24\x28\x00\x00\x00\x00")  # mov qword [rsp+0x28], 0
    c.rip(b"\xff\x15", slots["RtlUnwindEx"])     # call [RtlUnwindEx]
    c.emit(b"\xcc")
    c.label("end")
    return c


def unwind_info(prolog: int, codes: list, handler=None) -> bytes:
    flags = 1 if handler is not None else 0      # UNW_FLAG_EHANDLER
    data = bytes([1 | flags << 3, prolog, len(codes), 0])
    for offset, op, info in codes:
        data += bytes([offset, op | info << 4])
    if len(codes) % 2:
        data += b"\0\0"
    if handler is not None:
        data += struct.pack("<I", handler)
    return data


def build() -> bytes:
    section = bytearray(SECTION_SIZE)
    slots = build_imports(section)
    main = build_main(slots)
    main_code = main.resolve()
    fault = build_fault()
    fault_code = fault.resolve()
    handler = build_handler(slots, main)
    handler_code = handler.resolve()
    assert len(main_code) <= FAULT - MAIN and len(fault_code) <= HANDLER - FAULT
    section[MAIN:MAIN + len(main_code)] = main_code
    section[FAULT:FAULT + len(fault_code)] = fault_code
    section[HANDLER:HANDLER + len(handler_code)] = handler_code

    # Unwind information: UWOP_ALLOC_SMALL = 2, UWOP_PUSH_NONVOL = 0 (rbx = 3).
    infos = [
        (MAIN, main.labels["end"], unwind_info(4, [(4, 2, 0x28 // 8 - 1)], rva(HANDLER))),
        (FAULT, fault.labels["end"], unwind_info(5, [(5, 2, 0x20 // 8 - 1), (1, 0, 3)])),
        (HANDLER, handler.labels["end"], unwind_info(4, [(4, 2, 0x38 // 8 - 1)])),
    ]
    cursor = XDATA
    pdata = b""
    for begin, end, info in infos:
        section[cursor:cursor + len(info)] = info
        pdata += struct.pack("<III", rva(begin), rva(end), rva(cursor))
        cursor = (cursor + len(info) + 3) & ~3
    assert cursor <= PDATA
    section[PDATA:PDATA + len(pdata)] = pdata

    headers = bytearray(SIZE_OF_HEADERS)
    headers[0:2] = b"MZ"
    headers[0x3C:0x40] = struct.pack("<I", PE_OFFSET)
    at = PE_OFFSET
    headers[at:at + 4] = b"PE\0\0"
    # COFF: AMD64, one section, PE32+ optional header; executable, large
    # address aware, relocations stripped (it stays at its base).
    headers[at + 4:at + 24] = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, 0xF0, 0x0023)
    o = at + 24
    optional = struct.pack("<HBBIIIII", 0x20B, 14, 0, SECTION_SIZE, 0, 0, rva(MAIN), SECTION_RVA)
    optional += struct.pack("<QII", IMAGE_BASE, 0x1000, 0x200)
    optional += struct.pack("<HHHHHHI", 6, 0, 0, 0, 6, 0, 0)
    optional += struct.pack("<IIIHH", SIZE_OF_IMAGE, SIZE_OF_HEADERS, 0, 3, 0x8120)
    optional += struct.pack("<QQQQII", 0x100000, 0x1000, 0x100000, 0x1000, 0, 16)
    directories = [(0, 0)] * 16
    directories[1] = (rva(IMPORTS), 40)
    directories[3] = (rva(PDATA), len(pdata))
    optional += b"".join(struct.pack("<II", *d) for d in directories)
    assert len(optional) == 0xF0
    headers[o:o + 0xF0] = optional
    sections = o + 0xF0
    headers[sections:sections + 40] = b".text\0\0\0" + struct.pack(
        "<IIIIIIHHI", SECTION_SIZE, SECTION_RVA, SECTION_SIZE, SECTION_RAW, 0, 0, 0, 0, 0xE0000060)
    return bytes(headers) + bytes(section)


def main() -> None:
    OUTPUT.write_bytes(build())
    print(f"wrote {OUTPUT}")


if __name__ == "__main__":
    main()
