#!/usr/bin/env python3
"""Generate the deterministic FAT32 image used by the desktop Explorer demo.

The volume is not a fixture with hardcoded shortcut data: it contains real
files, including real PE binaries. The desktop scans those directory entries
and launches whichever program the user double-clicks.
"""

from pathlib import Path
import math
import struct

ROOT = Path(__file__).resolve().parent.parent
BPS = 512
SPB = 1
RESERVED = 4
FATS = 2
FAT_SECTORS = 64
TOTAL_SECTORS = 8192
DATA_START = RESERVED + FATS * FAT_SECTORS
CLUSTER_COUNT = (TOTAL_SECTORS - DATA_START) // SPB
CLUSTER_SIZE = BPS * SPB
ROOT_CLUSTER = 2
EOC = 0x0FFFFFFF
DIR_ENTRY_SIZE = 32
LFN_OFFSETS = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30]

README_DATA = b"Welcome to the OTW FAT32 test volume.\r\nExplorer is read-only.\r\n"
NOTES_DATA = b"FAT32 directories and files are real.\r\n"
SYSTEM_DATA = b"[boot]\r\nshell=Explorer\r\n"
LONG_DATA = bytes((index * 7) % 256 for index in range(BPS)) + bytes(
    (index * 11) % 256 for index in range(188)
)

# Real binaries placed on the demo volume. The launcher discovers them by
# walking the image, just like any other file a user copies onto the disk.
PROGRAMS = [
    ("HELLO.EXE", ROOT / "binaries" / "guests" / "HELLO.EXE"),
    ("NOTEPAD.EXE", ROOT / "binaries" / "win95" / "NOTEPAD.EXE"),
    ("PBRUSH.EXE", ROOT / "binaries" / "win95" / "PBRUSH.EXE"),
    ("MSPAINT.EXE", ROOT / "binaries" / "win95" / "MSPAINT.EXE"),
    ("MFC30.DLL", ROOT / "binaries" / "win95" / "MFC30.DLL"),
    ("MFCO30.DLL", ROOT / "binaries" / "win95" / "MFCO30.DLL"),
    ("MFCANS32.DLL", ROOT / "binaries" / "win95" / "MFCANS32.dll"),
    ("WINSOCK.EXE", ROOT / "binaries" / "guests" / "WINSOCK.EXE"),
]

OUT = ROOT / "binaries" / "disks" / "OTWDISK.IMG"


def cluster_offset(cluster):
    return (DATA_START + (cluster - 2) * SPB) * BPS


def fat_offset(fat, cluster):
    return (RESERVED + fat * FAT_SECTORS) * BPS + cluster * 4


def put_u16(image, offset, value):
    image[offset : offset + 2] = struct.pack("<H", value)


def put_u32(image, offset, value):
    image[offset : offset + 4] = struct.pack("<I", value)


def name11(text):
    raw = text.encode("ascii")
    if len(raw) > 11:
        raise ValueError(f"short name is longer than 11 bytes: {text}")
    return raw + b" " * (11 - len(raw))


def dos_short_name(name):
    base, separator, extension = name.rpartition(".")
    if not separator:
        return name.upper()[:8].ljust(8)
    return base.upper()[:8].ljust(8) + extension.upper()[:3].ljust(3)


def checksum(short_name):
    value = 0
    for byte in name11(short_name):
        value = ((((value & 1) << 7) | (value >> 1)) + byte) & 0xFF
    return value


def short_record(name, attrs, entry_cluster, size=0):
    record = bytearray(DIR_ENTRY_SIZE)
    record[0:11] = name11(name)
    record[11] = attrs
    record[20:22] = struct.pack("<H", entry_cluster >> 16)
    record[26:28] = struct.pack("<H", entry_cluster & 0xFFFF)
    record[28:32] = struct.pack("<I", size)
    return bytes(record)


def lfn_records(name, short_name):
    encoded = name.encode("utf-16-le")
    units = list(struct.unpack(f"<{len(encoded) // 2}H", encoded))
    total = math.ceil(len(units) / 13)
    records = []
    for ordinal in range(total, 0, -1):
        chunk = units[(ordinal - 1) * 13 : ordinal * 13]
        chars = chunk + [0xFFFF] * (13 - len(chunk))
        if ordinal == total and len(chunk) < 13:
            chars[len(chunk)] = 0x0000
        record = bytearray(DIR_ENTRY_SIZE)
        record[0] = ordinal | (0x40 if ordinal == total else 0)
        record[11] = 0x0F
        record[13] = checksum(short_name)
        for slot, unit in enumerate(chars):
            put_u16(record, LFN_OFFSETS[slot], unit)
        records.append(bytes(record))
    return records


class Directory:
    """A directory's 32-byte entry records, built before its cluster is written."""

    def __init__(self, cluster, parent):
        self.records = []
        self.add_short(".", 0x10, cluster)
        self.add_short("..", 0x10, parent)

    def add_short(self, name, attrs, cluster, size=0):
        self.records.append(short_record(name, attrs, cluster, size))

    def add_long(self, name, short_name, attrs, cluster, size=0):
        self.records.extend(lfn_records(name, short_name))
        self.add_short(short_name, attrs, cluster, size)

    def bytes(self):
        return b"".join(self.records)


def set_fat(image, cluster, value):
    for fat in range(FATS):
        put_u32(image, fat_offset(fat, cluster), value)


def write_cluster(image, cluster, data):
    offset = cluster_offset(cluster)
    image[offset : offset + len(data)] = data


class Allocator:
    """Sequential FAT32 cluster allocator for the generated image."""

    def __init__(self, image):
        self.image = image
        self.next = ROOT_CLUSTER + 1

    def chain(self, length):
        count = max(1, math.ceil(length / CLUSTER_SIZE))
        chain = []
        for _ in range(count):
            if self.next + 2 > CLUSTER_COUNT:
                raise ValueError("demo disk image is full")
            chain.append(self.next)
            self.next += 1
        for index, cluster in enumerate(chain):
            set_fat(
                self.image,
                cluster,
                chain[index + 1] if index + 1 < len(chain) else EOC,
            )
        return chain

    def write(self, chain, data):
        for index, cluster in enumerate(chain):
            start = index * CLUSTER_SIZE
            write_cluster(self.image, cluster, data[start : start + CLUSTER_SIZE])

    def allocate(self, data):
        chain = self.chain(len(data))
        self.write(chain, data)
        return chain[0]


def main():
    image = bytearray(TOTAL_SECTORS * BPS)
    image[0:3] = b"\xeb\x3c\x90"
    image[3:11] = b"OTWTEST "
    put_u16(image, 11, BPS)
    image[13] = SPB
    put_u16(image, 14, RESERVED)
    image[16] = FATS
    put_u16(image, 17, 0)
    put_u16(image, 19, 0)
    image[21] = 0xF8
    put_u16(image, 22, 0)
    put_u32(image, 32, TOTAL_SECTORS)
    put_u32(image, 36, FAT_SECTORS)
    put_u32(image, 44, ROOT_CLUSTER)
    put_u16(image, 48, 1)
    put_u16(image, 50, 6)
    image[71:82] = name11("OTW DISK")
    image[82:90] = b"FAT32   "
    image[510] = 0x55
    image[511] = 0xAA

    put_u32(image, BPS, 0x41615252)
    put_u32(image, BPS + 484, 0x61417272)
    put_u32(image, BPS + 508, 0xAA550000)

    set_fat(image, 0, 0x0FFFFFF8)
    set_fat(image, 1, EOC)
    set_fat(image, ROOT_CLUSTER, EOC)

    allocator = Allocator(image)
    docs_chain = allocator.chain(0)
    windows_chain = allocator.chain(0)

    readme_cluster = allocator.allocate(README_DATA)
    notes_cluster = allocator.allocate(NOTES_DATA)
    system_cluster = allocator.allocate(SYSTEM_DATA)
    long_chain = allocator.chain(len(LONG_DATA))
    allocator.write(long_chain, LONG_DATA)
    loaded = []
    for name, source in PROGRAMS:
        data = source.read_bytes()
        loaded.append((name, data, allocator.allocate(data)))

    root = Directory(ROOT_CLUSTER, ROOT_CLUSTER)
    root.add_long("Documents", "DOCUMEN~1", 0x10, docs_chain[0])
    root.add_long("README.TXT", "README~1TXT", 0x20, readme_cluster, len(README_DATA))
    root.add_long("WINDOWS", "WINDOWS  ", 0x10, windows_chain[0])
    root.add_long("LONG.TXT", "LONG    TXT", 0x20, long_chain[0], len(LONG_DATA))
    for name, data, cluster in loaded:
        root.add_short(dos_short_name(name), 0x20, cluster, len(data))
    # The root directory grows past one cluster as programs are added.
    root_bytes = root.bytes()
    root_chain = [ROOT_CLUSTER]
    if len(root_bytes) > CLUSTER_SIZE:
        extra = allocator.chain(len(root_bytes) - CLUSTER_SIZE)
        set_fat(image, ROOT_CLUSTER, extra[0])
        root_chain += extra
    allocator.write(root_chain, root_bytes)

    docs = Directory(docs_chain[0], ROOT_CLUSTER)
    docs.add_long("NOTES.TXT", "NOTES~1TXT", 0x20, notes_cluster, len(NOTES_DATA))
    allocator.write(docs_chain, docs.bytes())

    windows = Directory(windows_chain[0], ROOT_CLUSTER)
    windows.add_long("SYSTEM.INI", "SYSTEM~1INI", 0x20, system_cluster, len(SYSTEM_DATA))
    allocator.write(windows_chain, windows.bytes())

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(image)
    assert len(image) == TOTAL_SECTORS * BPS
    assert FAT_SECTORS * BPS // 4 >= CLUSTER_COUNT + 2
    print(f"wrote {OUT} ({len(image)} bytes, {CLUSTER_COUNT} clusters)")
    print(f"programs: {', '.join(name for name, _, _ in loaded)}")


if __name__ == "__main__":
    main()
