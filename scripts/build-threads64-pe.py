#!/usr/bin/env python3
"""Build THREADS64.EXE, a tiny 64-bit program for thread synchronisation tests.

No cross-compiler is required: machine code, imports and headers are emitted
directly (as for SEH64.EXE).

The main thread starts WORKERS threads; each adds 1 to a shared counter
ITERATIONS times under a one-byte mutex built like Rust's standard mutex on
Windows: 0 unlocked, 1 locked, 2 locked with waiters; a contended locker
swaps in 2 and sleeps with WaitOnAddress, an unlocker that finds 2 wakes one
with WakeByAddressSingle. The main thread waits for all workers
(WaitForMultipleObjects) and exits with the counter: WORKERS * ITERATIONS
when no increment and no wake-up was lost.
"""

from pathlib import Path
import struct

ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "binaries" / "guests" / "THREADS64.EXE"

WORKERS = 4
ITERATIONS = 20000

IMAGE_BASE = 0x40_0000
PE_OFFSET = 0x80
SECTION_RVA = 0x1000
SECTION_RAW = 0x200
SECTION_SIZE = 0x800
SIZE_OF_IMAGE = 0x2000
SIZE_OF_HEADERS = 0x200

CODE = 0x000
IMPORTS = 0x400
NAMES = 0x480
LOCK = 0x700      # the mutex byte
COUNTER = 0x708   # u64
HANDLES = 0x710   # WORKERS handles

FUNCTIONS = ["CreateThread", "WaitForMultipleObjects", "ExitProcess", "WaitOnAddress", "WakeByAddressSingle"]


def rva(offset: int) -> int:
    return SECTION_RVA + offset


class Code:
    def __init__(self, base: int):
        self.base = base
        self.bytes = bytearray()
        self.fixups = []  # (position of disp32, target offset or label, bytes after disp)
        self.labels = {}

    def label(self, name: str) -> None:
        self.labels[name] = self.base + len(self.bytes)

    def emit(self, data: bytes) -> None:
        self.bytes += data

    def rel(self, prefix: bytes, target, after: int = 0) -> None:
        """An instruction with a RIP-relative (or branch) disp32 to `target`."""
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
    assert iat + 8 * (len(FUNCTIONS) + 1) <= LOCK
    struct.pack_into("<IIIII", section, IMPORTS, rva(lookup), 0, 0, dll_name, rva(iat))
    return slots


def build_code(slots: dict) -> Code:
    c = Code(CODE)
    # ── main ──
    c.label("main")
    c.emit(b"\x48\x83\xec\x48")                      # sub rsp, 0x48
    for index in range(WORKERS):
        # CreateThread(0, 0, worker, 0, 0, 0)
        c.emit(b"\x31\xc9")                          # xor ecx, ecx
        c.emit(b"\x31\xd2")                          # xor edx, edx
        c.rel(b"\x4c\x8d\x05", "worker")             # lea r8, [rip+worker]
        c.emit(b"\x45\x31\xc9")                      # xor r9d, r9d
        c.emit(b"\x48\xc7\x44\x24\x20\x00\x00\x00\x00")  # mov qword [rsp+0x20], 0
        c.emit(b"\x48\xc7\x44\x24\x28\x00\x00\x00\x00")  # mov qword [rsp+0x28], 0
        c.rel(b"\xff\x15", slots["CreateThread"])    # call [CreateThread]
        c.rel(b"\x48\x89\x05", HANDLES + 8 * index)  # mov [rip+handle], rax
    # WaitForMultipleObjects(WORKERS, handles, TRUE, INFINITE)
    c.emit(b"\xb9" + struct.pack("<I", WORKERS))     # mov ecx, WORKERS
    c.rel(b"\x48\x8d\x15", HANDLES)                  # lea rdx, [rip+handles]
    c.emit(b"\x41\xb8\x01\x00\x00\x00")              # mov r8d, 1
    c.emit(b"\x41\xb9\xff\xff\xff\xff")              # mov r9d, -1
    c.rel(b"\xff\x15", slots["WaitForMultipleObjects"])
    c.rel(b"\x48\x8b\x0d", COUNTER)                  # mov rcx, [rip+counter]
    c.rel(b"\xff\x15", slots["ExitProcess"])
    c.emit(b"\xcc")

    # ── worker: ITERATIONS locked increments ──
    c.label("worker")
    c.emit(b"\x53")                                  # push rbx
    c.emit(b"\x56")                                  # push rsi
    c.emit(b"\x48\x83\xec\x28")                      # sub rsp, 0x28
    c.emit(b"\xbe" + struct.pack("<I", ITERATIONS))  # mov esi, ITERATIONS
    c.label("loop")
    c.rel(b"\xe8", "lock")                           # call lock
    c.rel(b"\x48\xff\x05", COUNTER)                  # inc qword [rip+counter]
    c.rel(b"\xe8", "unlock")                         # call unlock
    c.emit(b"\xff\xce")                              # dec esi
    c.rel(b"\x0f\x85", "loop")                       # jnz loop
    c.emit(b"\x31\xc0")                              # xor eax, eax
    c.emit(b"\x48\x83\xc4\x28")                      # add rsp, 0x28
    c.emit(b"\x5e")                                  # pop rsi
    c.emit(b"\x5b")                                  # pop rbx
    c.emit(b"\xc3")                                  # ret

    # ── lock ──
    c.label("lock")
    c.emit(b"\x48\x83\xec\x38")                      # sub rsp, 0x38
    c.emit(b"\xb0\x00")                              # mov al, 0
    c.emit(b"\xb1\x01")                              # mov cl, 1
    c.rel(b"\xf0\x0f\xb0\x0d", LOCK)                 # lock cmpxchg [rip+lock], cl
    c.rel(b"\x0f\x84", "locked")                     # jz locked
    c.label("contended")
    c.emit(b"\xb0\x02")                              # mov al, 2
    c.rel(b"\x86\x05", LOCK)                         # xchg [rip+lock], al
    c.emit(b"\x84\xc0")                              # test al, al
    c.rel(b"\x0f\x84", "locked")                     # jz locked (was free: ours)
    # WaitOnAddress(&lock, &two, 1, INFINITE)
    c.emit(b"\xc6\x44\x24\x30\x02")                  # mov byte [rsp+0x30], 2
    c.rel(b"\x48\x8d\x0d", LOCK)                     # lea rcx, [rip+lock]
    c.emit(b"\x48\x8d\x54\x24\x30")                  # lea rdx, [rsp+0x30]
    c.emit(b"\x41\xb8\x01\x00\x00\x00")              # mov r8d, 1
    c.emit(b"\x41\xb9\xff\xff\xff\xff")              # mov r9d, -1
    c.rel(b"\xff\x15", slots["WaitOnAddress"])
    c.rel(b"\xe9", "contended")                      # jmp contended
    c.label("locked")
    c.emit(b"\x48\x83\xc4\x38")                      # add rsp, 0x38
    c.emit(b"\xc3")                                  # ret

    # ── unlock ──
    c.label("unlock")
    c.emit(b"\x48\x83\xec\x28")                      # sub rsp, 0x28
    c.emit(b"\xb0\x00")                              # mov al, 0
    c.rel(b"\x86\x05", LOCK)                         # xchg [rip+lock], al
    c.emit(b"\x3c\x02")                              # cmp al, 2
    c.rel(b"\x0f\x85", "unlocked")                   # jne unlocked
    c.rel(b"\x48\x8d\x0d", LOCK)                     # lea rcx, [rip+lock]
    c.rel(b"\xff\x15", slots["WakeByAddressSingle"])
    c.label("unlocked")
    c.emit(b"\x48\x83\xc4\x28")                      # add rsp, 0x28
    c.emit(b"\xc3")                                  # ret
    return c


def build() -> bytes:
    section = bytearray(SECTION_SIZE)
    slots = build_imports(section)
    code = build_code(slots)
    machine = code.resolve()
    assert len(machine) <= IMPORTS - CODE
    section[CODE:CODE + len(machine)] = machine

    headers = bytearray(SIZE_OF_HEADERS)
    headers[0:2] = b"MZ"
    headers[0x3C:0x40] = struct.pack("<I", PE_OFFSET)
    at = PE_OFFSET
    headers[at:at + 4] = b"PE\0\0"
    headers[at + 4:at + 24] = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, 0xF0, 0x0023)
    o = at + 24
    optional = struct.pack("<HBBIIIII", 0x20B, 14, 0, SECTION_SIZE, 0, 0, rva(code.labels["main"]), SECTION_RVA)
    optional += struct.pack("<QII", IMAGE_BASE, 0x1000, 0x200)
    optional += struct.pack("<HHHHHHI", 6, 0, 0, 0, 6, 0, 0)
    optional += struct.pack("<IIIHH", SIZE_OF_IMAGE, SIZE_OF_HEADERS, 0, 3, 0x8120)
    optional += struct.pack("<QQQQII", 0x100000, 0x1000, 0x100000, 0x1000, 0, 16)
    directories = [(0, 0)] * 16
    directories[1] = (rva(IMPORTS), 40)
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
