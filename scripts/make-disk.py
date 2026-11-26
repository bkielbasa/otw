#!/usr/bin/env python3
"""Make a GPT disk image of FAT32 partitions filled from host directories,
with no root rights and nothing but the standard library.

    make-disk.py OUTPUT PARTITION...

Each PARTITION is KIND:LABEL:SIZE_MIB:DIRECTORY, KIND being "esp" (an EFI
system partition: what UEFI boots, EFI/BOOT/BOOTX64.EFI) or "data" (a basic
data partition). SIZE_MIB "auto" makes room for the directory and a quarter
more. Long file names are kept (VFAT long name entries).
"""

import os
import struct
import sys
import uuid
import zlib

SECTOR = 512
ESP_TYPE = uuid.UUID("C12A7328-F81F-11D2-BA4B-00A0C93EC93B")
DATA_TYPE = uuid.UUID("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7")
MIB = 1024 * 1024


# ── FAT32 ──

class Fat32:
    def __init__(self, size_bytes, label, out, offset):
        self.out, self.offset = out, offset
        self.sectors = size_bytes // SECTOR
        # Clusters must number at least 65525 for FAT32.
        for spc in (1, 2, 4, 8, 16, 32, 64):
            if self.sectors // spc < 65525 * 2:
                break
        self.spc = spc
        self.reserved = 32
        clusters = self.sectors // spc
        self.fat_sectors = (clusters * 4 + SECTOR - 1) // SECTOR
        self.data_start = self.reserved + 2 * self.fat_sectors
        self.clusters = (self.sectors - self.data_start) // spc
        if self.clusters < 65525:
            raise SystemExit(f"partition {label!r} is too small for FAT32 ({size_bytes // MIB} MiB)")
        self.cluster_bytes = spc * SECTOR
        self.label = label.upper()[:11].ljust(11).encode("ascii")
        self.fat = [0x0FFFFFF8, 0x0FFFFFFF]
        self.next_cluster = 2

    def allocate(self, nbytes):
        """A chain of clusters for `nbytes` (at least one); returns the first."""
        count = max(1, (nbytes + self.cluster_bytes - 1) // self.cluster_bytes)
        first = self.next_cluster
        if first + count - 2 > self.clusters:
            raise SystemExit("the partition is full")
        for c in range(first, first + count):
            self.fat.append(c + 1 if c < first + count - 1 else 0x0FFFFFFF)
        self.next_cluster += count
        return first

    def write_chain(self, first, payload):
        """Write `payload` to its (contiguous) clusters, straight to the disk."""
        self.out.seek(self.offset + (self.data_start + (first - 2) * self.spc) * SECTOR)
        self.out.write(payload)

    def boot_sector(self, volume_id):
        b = bytearray(SECTOR)
        b[0:3] = b"\xEB\x58\x90"
        b[3:11] = b"OTW     "
        struct.pack_into("<HBHBHHBHHHII", b, 11, SECTOR, self.spc, self.reserved, 2, 0, 0, 0xF8, 0, 63, 255, 0, self.sectors)
        struct.pack_into("<IHHIHH", b, 36, self.fat_sectors, 0, 0, 2, 1, 6)
        struct.pack_into("<BBBI", b, 64, 0x80, 0, 0x29, volume_id)
        b[71:82] = self.label
        b[82:90] = b"FAT32   "
        b[510:512] = b"\x55\xAA"
        return bytes(b)

    def fsinfo(self):
        b = bytearray(SECTOR)
        struct.pack_into("<I", b, 0, 0x41615252)
        struct.pack_into("<I", b, 484, 0x61417272)
        struct.pack_into("<II", b, 488, self.clusters - (self.next_cluster - 2), self.next_cluster)
        b[510:512] = b"\x55\xAA"
        return bytes(b)

    def finish(self):
        out, offset = self.out, self.offset
        volume_id = int.from_bytes(os.urandom(4), "little")
        boot = self.boot_sector(volume_id)
        for base in (0, 6):
            out.seek(offset + (base + 0) * SECTOR)
            out.write(boot)
            out.write(self.fsinfo())
            out.seek(offset + (base + 2) * SECTOR + 510)
            out.write(b"\x55\xAA")
        fat = struct.pack(f"<{len(self.fat)}I", *self.fat)
        for copy in range(2):
            out.seek(offset + (self.reserved + copy * self.fat_sectors) * SECTOR)
            out.write(fat)


def short_name(name, taken):
    """An 8.3 name for `name`, and whether it needs long name entries."""
    upper = name.upper()
    base, dot, ext = upper.rpartition(".") if "." in upper[1:] else (upper, "", "")
    valid = lambda s: all(c.isalnum() or c in "$%'-_@~`!(){}^#&" for c in s)
    if dot and name == upper and 0 < len(base) <= 8 and len(ext) <= 3 and valid(base) and valid(ext):
        return (base.ljust(8) + ext.ljust(3)).encode("ascii"), False
    if not dot and name == upper and 0 < len(base) <= 8 and valid(base):
        return base.ljust(11).encode("ascii"), False
    clean = lambda s: "".join(c if valid(c) and c.isascii() else "_" for c in s.replace(" ", "").replace(".", ""))
    stem, extension = clean(base)[:6] or "_", clean(ext)[:3]
    for n in range(1, 1000000):
        tail = f"~{n}"
        candidate = (stem[:8 - len(tail)] + tail).ljust(8) + extension.ljust(3)
        if candidate not in taken:
            return candidate.encode("ascii"), True
    raise SystemExit(f"too many names like {name}")


def lfn_checksum(short):
    total = 0
    for byte in short:
        total = (((total & 1) << 7) + (total >> 1) + byte) & 0xFF
    return total


def directory_entries(entries):
    """Directory records for (name, attribute, first cluster, size) entries."""
    out = bytearray()
    taken = set()
    for name, attribute, cluster, size in entries:
        if name in (".", ".."):
            short, long_name = name.encode().ljust(11), False
        else:
            short, long_name = short_name(name, taken)
        taken.add(short.decode("ascii"))
        if long_name:
            units = list(name.encode("utf-16-le"))
            chars = [units[i] | units[i + 1] << 8 for i in range(0, len(units), 2)]
            chars.append(0)
            while len(chars) % 13:
                chars.append(0xFFFF)
            pieces = [chars[i:i + 13] for i in range(0, len(chars), 13)]
            check = lfn_checksum(short)
            for index in range(len(pieces), 0, -1):
                piece = pieces[index - 1]
                order = index | (0x40 if index == len(pieces) else 0)
                record = struct.pack("<B5HBBB6HH2H", order, *piece[0:5], 0x0F, 0, check, *piece[5:11], 0, *piece[11:13])
                out += record
        out += struct.pack("<11sBBBHHHHHHHI", short, attribute, 0, 0, 0, 0x5B21, 0x5B21, cluster >> 16, 0, 0x5B21, cluster & 0xFFFF, size)
    return bytes(out)


def fill(fat, directories, cluster=None, parent=0):
    """Write the merged tree of `directories` (later ones win); returns the
    cluster of its records."""
    if isinstance(directories, str):
        directories = directories.split("+")
    found = {}
    for directory in directories:
        for name in os.listdir(directory):
            path = os.path.join(directory, name)
            if os.path.islink(path):
                continue
            key = name.lower()
            if os.path.isdir(path):
                previous = found.get(key)
                paths = previous[2] if previous and previous[1] == 0x10 else []
                found[key] = (name, 0x10, paths + [path], 0)
            elif os.path.isfile(path):
                found[key] = (name, 0x20, path, os.path.getsize(path))
    children = [found[k] for k in sorted(found)]
    # Room for this directory's records: allocate before the children so
    # its cluster number is known to them ("..").
    estimate = 32 * (3 + sum(1 + (len(n.encode("utf-16-le")) // 2 + 13) // 13 for n, *_ in children))
    if cluster is None:
        cluster = fat.allocate(estimate)
    entries = [] if parent == 0 and cluster == 2 else [(".", 0x10, cluster, 0), ("..", 0x10, parent, 0)]
    for name, attribute, path, size in children:
        if attribute == 0x10:
            child = fill(fat, path, None, cluster if cluster != 2 else 0)
            entries.append((name, 0x10, child, 0))
        else:
            first = fat.allocate(size) if size else 0
            if size:
                with open(path, "rb") as f:
                    fat.write_chain(first, f.read())  # one file at a time
            entries.append((name, 0x20, first, size))
    records = directory_entries(entries)
    if parent == 0 and cluster == 2:
        records = struct.pack("<11sBBBHHHHHHHI", fat.label, 0x08, 0, 0, 0, 0, 0, 0, 0, 0x5B21, 0, 0) + records
    assert len(records) <= estimate + 32, (directory, len(records), estimate)
    fat.write_chain(cluster, records)
    return cluster


def tree_bytes(directories):
    total = 0
    for root, dirs, files in (w for d in directories.split("+") for w in os.walk(d)):
        total += 4096 * (len(files) + len(dirs) + 1)
        for f in files:
            path = os.path.join(root, f)
            if not os.path.islink(path):
                total += os.path.getsize(path)
    return total


# ── GPT ──

def gpt(out, total_sectors, partitions):
    entries = bytearray(128 * 128)
    for i, (kind, label, start, end) in enumerate(partitions):
        type_guid = ESP_TYPE if kind == "esp" else DATA_TYPE
        name = label.encode("utf-16-le")[:72]
        struct.pack_into("<16s16sQQQ72s", entries, i * 128, type_guid.bytes_le, uuid.uuid4().bytes_le, start, end, 0, name)
    entries_crc = zlib.crc32(entries)
    disk_guid = uuid.uuid4().bytes_le

    def header(current, backup, entries_lba):
        h = bytearray(92)
        struct.pack_into("<8sIIIIQQQQ16sQIII", h, 0, b"EFI PART", 0x00010000, 92, 0, 0,
                         current, backup, 34, total_sectors - 34, disk_guid, entries_lba, 128, 128, entries_crc)
        struct.pack_into("<I", h, 16, zlib.crc32(h))
        return bytes(h)

    mbr = bytearray(SECTOR)
    struct.pack_into("<B3sB3sII", mbr, 446, 0, b"\x00\x02\x00", 0xEE, b"\xFF\xFF\xFF", 1, min(total_sectors - 1, 0xFFFFFFFF))
    mbr[510:512] = b"\x55\xAA"
    out.seek(0)
    out.write(mbr)
    out.write(header(1, total_sectors - 1, 2))
    out.seek(2 * SECTOR)
    out.write(entries)
    out.seek((total_sectors - 33) * SECTOR)
    out.write(entries)
    out.seek((total_sectors - 1) * SECTOR)
    out.write(header(total_sectors - 1, 1, total_sectors - 33))


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    output, specs = sys.argv[1], sys.argv[2:]
    layout = []
    lba = MIB // SECTOR
    for spec in specs:
        kind, label, size, directory = spec.split(":", 3)
        if kind not in ("esp", "data"):
            raise SystemExit(f"unknown partition kind {kind!r}")
        if size == "auto":
            size_mib = max(64, (tree_bytes(directory) * 5 // 4) // MIB + 32)
        else:
            size_mib = int(size)
        sectors = size_mib * MIB // SECTOR
        layout.append((kind, label, lba, lba + sectors - 1, directory))
        lba += sectors
    total_sectors = lba + MIB // SECTOR
    with open(output, "wb") as out:
        out.truncate(total_sectors * SECTOR)
        for kind, label, start, end, directory in layout:
            fat = Fat32((end - start + 1) * SECTOR, label, out, start * SECTOR)
            fill(fat, directory)
            fat.finish()
        gpt(out, total_sectors, [(k, l, s, e) for k, l, s, e, _ in layout])
    print(f"wrote {output} ({total_sectors * SECTOR // MIB} MiB)")


if __name__ == "__main__":
    main()
