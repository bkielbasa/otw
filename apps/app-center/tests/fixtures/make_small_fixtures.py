"""Regenerates sample.zip and twoblock.mszyml (run with python3 -I)."""
import os, struct, zipfile, zlib

here = os.path.dirname(os.path.abspath(__file__))

with zipfile.ZipFile(os.path.join(here, "sample.zip"), "w") as z:
    z.writestr("Public/index.db", b"pretend database " * 200, zipfile.ZIP_DEFLATED)
    z.writestr("AppxManifest.xml", b"<Package/>", zipfile.ZIP_STORED)

# An MSZIP buffer as written by the Windows Compression API, with two blocks.
# Every block starts with "CK" and is a raw deflate stream that may refer
# back into the previous block's output.
first = b"".join(b"line %d of the first block\n" % i for i in range(1500))
second = first[:4000] + b"and something new at the end\n"
c1 = zlib.compressobj(9, zlib.DEFLATED, -15)
b1 = b"CK" + c1.compress(first) + c1.flush()
c2 = zlib.compressobj(9, zlib.DEFLATED, -15, zdict=first[-32768:])
b2 = b"CK" + c2.compress(second) + c2.flush()
total = len(first) + len(second)
header = b"\x0a\x51\xe5\xc0" + b"\x18\x00\x00\x02" + struct.pack("<QQ", total, len(first))
with open(os.path.join(here, "twoblock.mszyml"), "wb") as f:
    f.write(header + struct.pack("<I", len(b1)) + b1 + struct.pack("<I", len(b2)) + b2)
with open(os.path.join(here, "twoblock.txt"), "wb") as f:
    f.write(first + second)
