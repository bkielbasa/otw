#!/usr/bin/env python3
"""Build OTWCAPTION.TTF, the icon font that stands in for Windows' caption
button glyphs ("Segoe Fluent Icons" / "Segoe MDL2 Assets"): programs that
draw their own title bars (Firefox) show minimise, maximise, restore and
close with these code points. Drawn here as simple outlines.

Requires fontTools. Writes binaries/fonts/OTWCAPTION.TTF.
"""
from pathlib import Path

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

OUTPUT = Path(__file__).resolve().parents[1] / "binaries" / "fonts" / "OTWCAPTION.TTF"
EM = 1000
T = 100  # stroke thickness


def rect(pen, x0, y0, x1, y1, outer=True):
    """A rectangle contour: clockwise (filled) or counter-clockwise (a hole)."""
    points = [(x0, y0), (x0, y1), (x1, y1), (x1, y0)]
    if not outer:
        points.reverse()
    pen.moveTo(points[0])
    for point in points[1:]:
        pen.lineTo(point)
    pen.closePath()


def frame(pen, x0, y0, x1, y1):
    rect(pen, x0, y0, x1, y1)
    rect(pen, x0 + T, y0 + T, x1 - T, y1 - T, outer=False)


def minimize(pen):
    rect(pen, 0, 450, 1000, 450 + T)


def maximize(pen):
    frame(pen, 0, 0, 1000, 1000)


def restore(pen):
    frame(pen, 0, 0, 800, 800)
    # The window behind: its top and right edges, joined to the front one.
    rect(pen, 200, 1000 - T, 1000, 1000)
    rect(pen, 1000 - T, 200, 1000, 1000 - T)
    rect(pen, 200, 800, 200 + T, 1000 - T)
    rect(pen, 800, 200, 1000 - T, 200 + T)


def close(pen):
    d = 70
    points = [(d, 0), (500, 500 - d), (1000 - d, 0), (1000, d), (500 + d, 500), (1000, 1000 - d),
              (1000 - d, 1000), (500, 500 + d), (d, 1000), (0, 1000 - d), (500 - d, 500), (0, d)]
    points.reverse()  # clockwise
    pen.moveTo(points[0])
    for point in points[1:]:
        pen.lineTo(point)
    pen.closePath()


ICONS = {
    0xE921: ("minimize", minimize), 0xEF2D: ("minimize.contrast", minimize),
    0xE922: ("maximize", maximize), 0xEF2E: ("maximize.contrast", maximize),
    0xE923: ("restore", restore), 0xEF2F: ("restore.contrast", restore),
    0xE8BB: ("close", close), 0xEF2C: ("close.contrast", close),
}


def build() -> None:
    names = [".notdef", "space"] + [name for name, _ in ICONS.values()]
    fb = FontBuilder(EM, isTTF=True)
    fb.setupGlyphOrder(names)
    fb.setupCharacterMap({0x20: "space", **{code: name for code, (name, _) in ICONS.items()}})
    glyphs = {".notdef": TTGlyphPen(None).glyph(), "space": TTGlyphPen(None).glyph()}
    for name, draw in ICONS.values():
        pen = TTGlyphPen(None)
        draw(pen)
        glyphs[name] = pen.glyph()
    fb.setupGlyf(glyphs)
    metrics = {name: (EM, 0) for name in names}
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=EM, descent=0)
    fb.setupNameTable({"familyName": "OTW Caption Icons", "styleName": "Regular"})
    fb.setupOS2(sTypoAscender=EM, sTypoDescender=0, usWinAscent=EM, usWinDescent=0)
    fb.setupPost()
    fb.save(str(OUTPUT))
    print(f"wrote {OUTPUT}")


if __name__ == "__main__":
    build()
