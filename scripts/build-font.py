#!/usr/bin/env python3
"""Render the OTW proportional bitmap UI font from DejaVu Sans.

Output format (little endian):
  b"OTWF", u8 face_count
  per face: u8 bold, u8 cell_height, u8 ascent, u8 reserved
            then for codes 32..=255: u8 advance, followed by
            cell_height rows of ceil(advance/8) bytes, MSB = leftmost pixel.
Glyphs are rendered without anti-aliasing, like Windows 95 raster fonts.
"""
import sys
from PIL import Image, ImageDraw, ImageFont

REGULAR = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
# Cell heights matching common Win95 logical font sizes at 96 DPI.
HEIGHTS = [11, 13, 16, 19, 24, 32, 40, 52]


def face(path, cell, bold):
    # Pick the largest point size whose ascent+descent fits the cell.
    size = cell
    while size > 4:
        font = ImageFont.truetype(path, size)
        ascent, descent = font.getmetrics()
        if ascent + descent <= cell + 1:
            break
        size -= 1
    ascent, descent = font.getmetrics()
    ascent = min(ascent, cell)
    out = bytearray([1 if bold else 0, cell, ascent, 0])
    for code in range(32, 256):
        ch = bytes([code]).decode("latin-1")
        advance = max(1, round(font.getlength(ch)))
        if code == 32:
            advance = max(advance, cell // 4)
        if code == 0xA0:
            advance = max(advance, cell // 4)
        image = Image.new("1", (advance + 8, cell + 4), 0)
        draw = ImageDraw.Draw(image)
        draw.text((0, 0), ch, font=font, fill=1)
        stride = (advance + 7) // 8
        out.append(min(advance, 255))
        for y in range(cell):
            row = bytearray(stride)
            for x in range(advance):
                if image.getpixel((x, y)):
                    row[x // 8] |= 0x80 >> (x % 8)
            out += row
    return out


def main():
    target = sys.argv[1] if len(sys.argv) > 1 else "binaries/fonts/OTWSANS.FNT"
    faces = [(path, cell, bold) for bold, path in ((False, REGULAR), (True, BOLD)) for cell in HEIGHTS]
    blob = bytearray(b"OTWF") + bytes([len(faces)])
    for path, cell, bold in faces:
        blob += face(path, cell, bold)
    open(target, "wb").write(blob)
    print(f"{target}: {len(faces)} faces, {len(blob)} bytes")


if __name__ == "__main__":
    main()
