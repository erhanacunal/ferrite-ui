#!/usr/bin/env python3
"""
TrueType to Ferrite sparse font converter.

VERBS
  resource  Write a binary font resource (.bin) for device flash.
  code      Write a Rust source (.rs) for ROM-embedded fonts.

BINARY RESOURCE LAYOUT  (sparse format, N = num_glyphs):
  [0..2]         num_glyphs   u16 LE
  [2]            y_advance    u8
  [3]            font_id      u8
  [4..4+N*2]     codepoints   N x u16 LE, sorted ascending
  [4+N*2..4+N*9] glyphs       N x 7 bytes
                              (bitmapOffset u16 LE, width u8, height u8,
                               xAdvance u8, xOffset i8, yOffset i8)
  [4+N*9..]      bitmap       1-bit packed, MSB first;
                              each glyph padded to a byte boundary

RUST CODE OUTPUT  (arrays for use as an embedded ROM font):
  pub const Y_ADVANCE:  u8           = ...;
  pub static CODEPOINTS: [u16; N]    = [...];
  pub static BITMAP:    [u8;  M]     = [...];
  pub static GLYPHS:    [GfxGlyph; N] = [...];

RANGE SPEC  (-r / --ranges)
  Comma-separated codepoints and/or inclusive ranges.
  Tokens: decimal (32), hex (0x011E), or char literal ('A').
  Maximum codepoint: 0xFFFF (Unicode BMP).
  Examples:
    32-127
    32-127,0xC7,0xD6,0x011E-0x011F
    32-127,0xC7,0xD6,0xDC,0xE7,0xF6,0xFC,0x011E-0x011F,0x0130-0x0131,0x015E-0x015F

Requires: freetype-py  (pip install freetype-py)
"""

import argparse
import os
import struct
import sys

import freetype
from freetype import (
    FT_LOAD_TARGET_MONO,
    FT_RENDER_MODE_MONO,
    FT_Property_Set,
    FT_UInt,
)

DPI = 96
TT_INTERPRETER_VERSION_35 = 35
BITMAP_COLS = 12
CP_COLS = 8


# ── helpers ───────────────────────────────────────────────────────────────────

def parse_ranges(spec):
    """Parse a range spec into a sorted list of unique codepoints (0..0xFFFF)."""
    def parse_one(tok):
        tok = tok.strip()
        if not tok:
            raise ValueError("empty token")
        if len(tok) == 3 and tok[0] == "'" and tok[2] == "'":
            return ord(tok[1])
        if tok.lower().startswith("0x"):
            return int(tok, 16)
        return int(tok, 10)

    result = set()
    for part in spec.split(","):
        part = part.strip()
        if not part:
            continue
        if "-" in part and not (part.startswith("'") and part.endswith("'")):
            a, _, b = part.partition("-")
            lo, hi = parse_one(a), parse_one(b)
            if hi < lo:
                lo, hi = hi, lo
            result.update(range(lo, hi + 1))
        else:
            result.add(parse_one(part))

    if not result:
        raise ValueError("no codepoints in range spec %r" % spec)
    for c in result:
        if not 0 <= c <= 0xFFFF:
            raise ValueError("codepoint %d out of 0..0xFFFF" % c)
    return sorted(result)


def _char_label(code):
    """Short printable label for a codepoint, e.g. \" ' '\"."""
    if 0x20 <= code <= 0x7E:
        return " '%s'" % chr(code)
    return ""


# ── rasterization ─────────────────────────────────────────────────────────────

def rasterize(font_path, size, codes):
    """
    Rasterize `codes` (sorted list of BMP codepoints) at `size` points.

    Returns (bitmap_bytes, table, y_advance):
      bitmap_bytes  bytes                -- packed 1-bit bitmap data
      table         list[(int, dict)]    -- (codepoint, glyph metrics)
                    metrics: bitmapOffset, width, height, xAdvance, xOffset, yOffset
      y_advance     int                  -- line height in pixels
    """
    library = freetype.FT_Library()
    if freetype.FT_Init_FreeType(freetype.byref(library)):
        raise RuntimeError("FreeType init failed")

    interp = FT_UInt(TT_INTERPRETER_VERSION_35)
    FT_Property_Set(library, b"truetype", b"interpreter-version",
                    freetype.byref(interp))

    face = freetype.FT_Face()
    path_bytes = (font_path.encode(sys.getfilesystemencoding())
                  if isinstance(font_path, str) else font_path)
    if freetype.FT_New_Face(library, path_bytes, 0, freetype.byref(face)):
        freetype.FT_Done_FreeType(library)
        raise RuntimeError("Cannot load font: %s" % font_path)

    freetype.FT_Set_Char_Size(face, size << 6, 0, DPI, 0)

    # MSB-first bit accumulator -> raw bytes
    bitmap_buf = bytearray()
    _bits = [0, 7]  # [current_byte, current_shift]

    def enbit(v):
        if v:
            _bits[0] |= 1 << _bits[1]
        _bits[1] -= 1
        if _bits[1] < 0:
            bitmap_buf.append(_bits[0])
            _bits[0] = 0
            _bits[1] = 7

    table = []
    bmap_offset = 0

    for code in codes:
        err = freetype.FT_Load_Char(face, code, FT_LOAD_TARGET_MONO)
        if err:
            sys.stderr.write("Warning: error %d loading U+%04X\n" % (err, code))
            table.append((code, _zero_glyph(bmap_offset)))
            continue

        err = freetype.FT_Render_Glyph(face.contents.glyph, FT_RENDER_MODE_MONO)
        if err:
            sys.stderr.write("Warning: error %d rendering U+%04X\n" % (err, code))
            table.append((code, _zero_glyph(bmap_offset)))
            continue

        ft_glyph = freetype.FT_Glyph()
        err = freetype.FT_Get_Glyph(face.contents.glyph, freetype.byref(ft_glyph))
        if err:
            sys.stderr.write("Warning: error %d getting glyph U+%04X\n" % (err, code))
            table.append((code, _zero_glyph(bmap_offset)))
            continue

        slot   = face.contents.glyph.contents
        bmp    = slot.bitmap
        bglyph = freetype.cast(
            ft_glyph, freetype.POINTER(freetype.FT_BitmapGlyphRec)).contents

        w      = bmp.width
        rows   = bmp.rows
        pitch  = bmp.pitch
        x_adv  = slot.advance.x >> 6
        x_off  = bglyph.left
        y_off  = 1 - bglyph.top

        table.append((code, {
            "bitmapOffset": bmap_offset,
            "width":        w,
            "height":       rows,
            "xAdvance":     x_adv,
            "xOffset":      x_off,
            "yOffset":      y_off,
        }))

        buf_len = pitch * rows if pitch > 0 else 0
        raw = bytes(freetype.string_at(bmp.buffer, buf_len)) if buf_len > 0 else b""

        for y in range(rows):
            for x in range(w):
                bi = x // 8
                bm = 0x80 >> (x & 7)
                ao = y * pitch + bi
                enbit(raw[ao] & bm if 0 <= ao < len(raw) else 0)

        # Pad to byte boundary
        tail = (w * rows) & 7
        if tail:
            for _ in range(8 - tail):
                enbit(0)

        bmap_offset += (w * rows + 7) // 8
        freetype.FT_Done_Glyph(ft_glyph)

    # Safety flush (should be a no-op when padding is correct)
    if _bits[1] != 7:
        bitmap_buf.append(_bits[0])

    metrics = face.contents.size.contents.metrics
    y_advance = (metrics.height >> 6) if metrics.height else (
        table[0][1]["height"] if table else 0)

    freetype.FT_Done_Face(face)
    freetype.FT_Done_FreeType(library)

    return bytes(bitmap_buf), table, y_advance


def _zero_glyph(bmap_offset):
    return {"bitmapOffset": bmap_offset,
            "width": 0, "height": 0,
            "xAdvance": 0, "xOffset": 0, "yOffset": 0}


# ── output: binary resource ───────────────────────────────────────────────────

def write_resource(bitmap, table, y_advance, font_id, out):
    """
    Write the binary font resource to `out` (binary-mode file or buffer).

    Layout:
      4-byte header  | N*2 codepoints | N*7 glyphs | bitmap
    """
    if not (0 <= y_advance <= 255):
        max_pt = int(255 * 72 / DPI)
        raise ValueError(
            f"y_advance={y_advance}px exceeds u8 max (255); "
            f"reduce point size — try ≤{max_pt}pt at {DPI} DPI")

    n = len(table)
    out.write(struct.pack("<HBB", n, y_advance, font_id))
    for code, _g in table:
        out.write(struct.pack("<H", code))
    for code, g in table:
        bmo = g["bitmapOffset"]
        w, h, xa = g["width"], g["height"], g["xAdvance"]
        xo, yo = g["xOffset"], g["yOffset"]
        if bmo > 65535:
            raise ValueError(
                f"U+{code:04X} bitmapOffset={bmo} exceeds u16 max (65535); "
                f"bitmap too large — use fewer/smaller glyphs")
        if w > 255:
            raise ValueError(f"U+{code:04X} width={w}px exceeds u8 max (255)")
        if h > 255:
            raise ValueError(f"U+{code:04X} height={h}px exceeds u8 max (255)")
        if not (0 <= xa <= 255):
            raise ValueError(f"U+{code:04X} xAdvance={xa}px out of u8 range 0-255")
        if not (-128 <= xo <= 127):
            raise ValueError(f"U+{code:04X} xOffset={xo}px out of i8 range -128..127")
        if not (-128 <= yo <= 127):
            raise ValueError(f"U+{code:04X} yOffset={yo}px out of i8 range -128..127")
        out.write(struct.pack("<HBBBbb", bmo, w, h, xa, xo, yo))
    out.write(bitmap)


# ── output: Rust source ───────────────────────────────────────────────────────

def write_code(bitmap, table, y_advance, out):
    """
    Write Rust source to `out` (text-mode file).

    Produces: Y_ADVANCE, CODEPOINTS, BITMAP, GLYPHS with fixed names.
    Intended use: save as src/embedded_font.rs (or similar), then
    call Font::from_embedded(&GLYPHS, &CODEPOINTS, &BITMAP, Y_ADVANCE).
    """
    n = len(table)
    m = len(bitmap)

    out.write("// Generated by ferrite_font_converter.py (code mode).\n")
    out.write("// %d glyphs, y_advance=%d, bitmap=%d bytes\n" % (n, y_advance, m))
    out.write("\n")
    out.write("use crate::font::GfxGlyph;\n")
    out.write("\n")
    out.write("pub const Y_ADVANCE: u8 = %d;\n" % y_advance)
    out.write("\n")

    # CODEPOINTS — CP_COLS per row
    out.write("pub static CODEPOINTS: [u16; %d] = [\n" % n)
    for i, (code, _g) in enumerate(table):
        if i % CP_COLS == 0:
            out.write("    ")
        out.write("0x%04X" % code)
        if i < n - 1:
            if (i + 1) % CP_COLS == 0:
                out.write(",\n")
            else:
                out.write(", ")
    out.write("\n];\n\n")

    # BITMAP — BITMAP_COLS bytes per row
    out.write("pub static BITMAP: [u8; %d] = [\n" % m)
    for i, b in enumerate(bitmap):
        if i % BITMAP_COLS == 0:
            out.write("    ")
        out.write("0x%02X" % b)
        if i < m - 1:
            if (i + 1) % BITMAP_COLS == 0:
                out.write(",\n")
            else:
                out.write(", ")
    out.write("\n];\n\n")

    # GLYPHS — one entry per line with codepoint comment
    out.write("pub static GLYPHS: [GfxGlyph; %d] = [\n" % n)
    for code, g in table:
        out.write(
            "    GfxGlyph::new(%5d, %3d, %3d, %3d, %4d, %4d),"
            " // U+%04X%s\n" % (
                g["bitmapOffset"], g["width"], g["height"],
                g["xAdvance"], g["xOffset"], g["yOffset"],
                code, _char_label(code),
            )
        )
    out.write("];\n")


# ── CLI ───────────────────────────────────────────────────────────────────────

def _common_args(p):
    p.add_argument("fontfile", help="path to .ttf font file")
    p.add_argument("size", type=int, help="point size")
    p.add_argument(
        "-r", "--ranges", metavar="SPEC",
        help="codepoint range spec (default: 32-126). "
             "E.g. '32-127,0xC7,0xD6,0x011E-0x011F'")
    p.add_argument("-o", "--output", metavar="FILE",
                   help="output file (default: stdout)")


def _resolve_codes(args):
    if args.ranges:
        return parse_ranges(args.ranges)
    return list(range(0x20, 0x7F))   # printable ASCII default


def cmd_resource(args):
    if args.font_id == 0:
        sys.exit("error: font-id 0 is reserved for the embedded font "
                 "(load_by_id always skips it)")
    codes = _resolve_codes(args)
    bitmap, table, y_advance = rasterize(args.fontfile, args.size, codes)
    n, m = len(table), len(bitmap)
    total = 4 + n * 9 + m
    sys.stderr.write("resource: %d glyphs, bitmap %d B, total %d B\n"
                     % (n, m, total))
    if args.output:
        with open(args.output, "wb") as f:
            write_resource(bitmap, table, y_advance, args.font_id, f)
    else:
        write_resource(bitmap, table, y_advance, args.font_id,
                       sys.stdout.buffer)


def cmd_code(args):
    codes = _resolve_codes(args)
    bitmap, table, y_advance = rasterize(args.fontfile, args.size, codes)
    n, m = len(table), len(bitmap)
    sys.stderr.write("code: %d glyphs, bitmap %d B\n" % (n, m))
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            write_code(bitmap, table, y_advance, f)
    else:
        write_code(bitmap, table, y_advance, sys.stdout)


def main(argv=None):
    p = argparse.ArgumentParser(
        prog="ferrite_font_converter.py",
        description="TrueType -> Ferrite sparse font converter",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    sub = p.add_subparsers(dest="verb", metavar="VERB")
    sub.required = True

    pr = sub.add_parser(
        "resource",
        help="write binary font resource for device flash",
        description="Write a binary .bin sparse font resource for device flash.",
    )
    _common_args(pr)
    pr.add_argument(
        "--font-id", type=lambda x: int(x, 0), required=True, metavar="ID",
        help="font ID stored in resource header (1-255; 0 is reserved for embedded)")

    pc = sub.add_parser(
        "code",
        help="write Rust source for ROM-embedded font",
        description=(
            "Write a Rust .rs source with Y_ADVANCE, CODEPOINTS, BITMAP, GLYPHS "
            "arrays. Use with Font::from_embedded(&GLYPHS, &CODEPOINTS, &BITMAP, "
            "Y_ADVANCE)."
        ),
    )
    _common_args(pc)

    args = p.parse_args(argv)
    if args.verb == "resource":
        cmd_resource(args)
    elif args.verb == "code":
        cmd_code(args)


if __name__ == "__main__":
    main()
