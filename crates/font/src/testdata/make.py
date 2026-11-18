"""Rebuild the font test fixtures. Needs fontTools and brotli (pip install fonttools brotli);
neither is part of the build.

    python3 make.py path/to/Figtree-Regular.ttf

- figtree-latin.woff: Figtree's ASCII letters as WOFF 1.0 (zlib tables).
- icons.ttf, icons.woff2: a two-glyph icon font of our own. U+E001 is a square, U+E002 a
  triangle, both 1000 units wide; the WOFF2 transforms glyf, loca and hmtx.
"""

import sys

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont


def figtree_latin(source):
    font = TTFont(source)
    options = Options()
    options.flavor = "woff"
    options.layout_features = []
    subsetter = Subsetter(options)
    subsetter.populate(unicodes=range(0x20, 0x7F))
    subsetter.subset(font)
    font.flavor = "woff"
    font.save("figtree-latin.woff")


def icons():
    def square():
        pen = TTGlyphPen(None)
        pen.moveTo((100, 0))
        pen.lineTo((100, 700))
        pen.lineTo((900, 700))
        pen.lineTo((900, 0))
        pen.closePath()
        return pen.glyph()

    def triangle():
        pen = TTGlyphPen(None)
        pen.moveTo((100, 0))
        pen.lineTo((500, 700))
        pen.lineTo((900, 0))
        pen.closePath()
        return pen.glyph()

    builder = FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder([".notdef", "space", "square", "triangle"])
    builder.setupCharacterMap({0x20: "space", 0xE001: "square", 0xE002: "triangle"})
    empty = TTGlyphPen(None).glyph()
    builder.setupGlyf(
        {".notdef": empty, "space": empty, "square": square(), "triangle": triangle()}
    )
    builder.setupHorizontalMetrics(
        {".notdef": (500, 0), "space": (250, 0), "square": (1000, 100), "triangle": (1000, 100)}
    )
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable({"familyName": "Broxee Icons", "styleName": "Regular"})
    builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
    builder.setupPost()
    builder.save("icons.ttf")

    from fontTools.ttLib import woff2

    woff2.compress("icons.ttf", "icons.woff2", transform_tables={"glyf", "loca", "hmtx"})


if __name__ == "__main__":
    figtree_latin(sys.argv[1])
    icons()
