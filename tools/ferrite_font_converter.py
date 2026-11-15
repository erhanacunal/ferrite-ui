#!/usr/bin/env python3
"""
TrueType to Ferrite sparse font converter.

VERBS
  resource  Write a binary font resource (.bin) for device flash.
  code      Write a Rust source (.rs) for ROM-embedded fonts.

BINARY RESOURCE LAYOUT

  V1 (1bpp monochrome, default):
    [0..1]         num_glyphs   u16 LE
    [2]            y_advance    u8
    [3]            font_id      u8
    [4..4+N*2]     codepoints   N x u16 LE, sorted ascending
    [4+N*2..4+N*9] glyphs       N x 7 bytes
    [4+N*9..]      bitmap       1-bit packed, MSB first

  V2 (anti-aliased, --bpp 2):
    [0..1]         magic        0x00, 0x02
    [2..3]         num_glyphs   u16 LE
    [4]            y_advance    u8
    [5]            font_id      u8
    [6]            bpp          u8 (2)
    [7..7+N*2]     codepoints   N x u16 LE, sorted ascending
    [7+N*2..7+N*9] glyphs       N x 7 bytes
    [7+N*9..]      bitmap       2bpp packed, MSB first; 4 pixels per byte;
                                each pixel = 2 bits: 0=0%, 1=33%, 2=67%, 3=100%
                                each glyph padded to a byte boundary

  V3 (wide metrics, --v3; ferrite-ui-xl devices only, caps.font_v3):
    [0..1]           magic        0x00, 0x03
    [2..3]           num_glyphs   u16 LE
    [4..5]           y_advance    u16 LE
    [6]              font_id      u8
    [7]              bpp          u8 (1 or 2)
    [8..10]          variants     bold, italic, bold italic font_id (0 = none;
                                  ferrite_build sets them from the project's
                                  font entries: "bold" / "italic" / "bold_italic")
    [11]             reserved     0
    [12..12+N*2]     codepoints   N x u16 LE, sorted ascending
    [12+N*2..12+N*16] glyphs      N x 14 bytes
    [12+N*16..]      bitmap       as V1 (bpp 1) / V2 (bpp 2)

  Glyph entry, V1/V2 (7 bytes):
    bitmapOffset u16 LE, width u8, height u8,
    xAdvance u8, xOffset i8, yOffset i8
  Glyph entry, V3 (14 bytes, LE):
    bitmapOffset u32, width u16, height u16,
    xAdvance u16, xOffset i16, yOffset i16
  V1/V2 limit glyphs to 255 px (offsets -128..127) and bitmaps to 64 KB:
  around 130pt at 96 DPI; V3 has no practical limit.

RUST CODE OUTPUT  (arrays for use as an embedded ROM font):
  pub const Y_ADVANCE:  u8           = ...;
  pub const BPP:        u8           = ...;
  pub static CODEPOINTS: [u16; N]    = [...];
  pub static BITMAP:    [u8;  M]     = [...];
  pub static GLYPHS:    [GfxGlyph; N] = [...];

RANGE SPEC  (-r / --ranges)
  Comma-separated codepoints and/or inclusive ranges.
  Tokens: decimal (32), hex (0x011E), or char literal ('A', also ',' and '-').
  Maximum codepoint: 0xFFFF (Unicode BMP).
  Examples:
    32-127
    32-127,0xC7,0xD6,0x011E-0x011F
    '0'-'9',':',' '
    32-127,0xC7,0xD6,0xDC,0xE7,0xF6,0xFC,0x011E-0x011F,0x0130-0x0131,0x015E-0x015F

Requires: freetype-py  (pip install freetype-py)
"""

import argparse
import io
import os
import struct
import sys

# freetype-py is only needed to rasterize: ferrite_build imports this module for
# the resource layout helpers (parse_header / read_resource / to_v3) without it.
try:
    import freetype
    from freetype import (
        FT_LOAD_DEFAULT,
        FT_LOAD_TARGET_MONO,
        FT_RENDER_MODE_MONO,
        FT_RENDER_MODE_NORMAL,
        FT_Property_Set,
        FT_UInt,
    )
except ImportError as _e:
    freetype = None
    _FREETYPE_ERROR = _e

DPI = 96
TT_INTERPRETER_VERSION_35 = 35
BITMAP_COLS = 12
CP_COLS = 8


# ── helpers ───────────────────────────────────────────────────────────────────

def parse_ranges(spec):
    """Parse a range spec into a sorted list of unique codepoints (0..0xFFFF).

    Items are separated by commas; an item is a value or ``lo-hi``. A value
    is decimal, hex (0x..) or a quoted character, which may itself be a comma
    or a dash: ``'0'-'9',':',','``.
    """
    pos = 0

    def skip_ws():
        nonlocal pos
        while pos < len(spec) and spec[pos].isspace():
            pos += 1

    def value():
        nonlocal pos
        skip_ws()
        if pos < len(spec) and spec[pos] == "'":
            if pos + 2 >= len(spec) or spec[pos + 2] != "'":
                raise ValueError("bad character literal at %d in %r" % (pos, spec))
            code = ord(spec[pos + 1])
            pos += 3
            return code
        start = pos
        while pos < len(spec) and spec[pos].isalnum():
            pos += 1
        tok = spec[start:pos]
        if not tok:
            raise ValueError("expected a value at %d in %r" % (start, spec))
        if tok.lower().startswith("0x"):
            return int(tok, 16)
        return int(tok, 10)

    result = set()
    while True:
        skip_ws()
        if pos >= len(spec):
            break
        if spec[pos] == ",":  # empty item
            pos += 1
            continue
        lo = hi = value()
        skip_ws()
        if pos < len(spec) and spec[pos] == "-":
            pos += 1
            hi = value()
            if hi < lo:
                lo, hi = hi, lo
        result.update(range(lo, hi + 1))
        skip_ws()
        if pos < len(spec):
            if spec[pos] != ",":
                raise ValueError("expected ',' at %d in %r" % (pos, spec))
            pos += 1

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


# ── 2bpp quantisation ─────────────────────────────────────────────────────────

def _gray_to_2bpp(gray8):
    """Quantise 8-bit gray (0–255) → 2-bit alpha (0–3)."""
    if gray8 < 64:
        return 0
    elif gray8 < 128:
        return 1
    elif gray8 < 192:
        return 2
    else:
        return 3


# ── rasterization ─────────────────────────────────────────────────────────────

def rasterize(font_path, size, codes, bpp=1):
    """
    Rasterize `codes` (sorted list of BMP codepoints) at `size` points.

    `bpp`: 1 = monochrome (FT_RENDER_MODE_MONO), 2 = 2bpp anti-aliased.

    Returns (bitmap_bytes, table, y_advance):
      bitmap_bytes  bytes                -- packed bitmap data
      table         list[(int, dict)]    -- (codepoint, glyph metrics)
                    metrics: bitmapOffset, width, height, xAdvance, xOffset, yOffset
      y_advance     int                  -- line height in pixels
    """
    if freetype is None:
        raise ImportError(f"freetype-py is required to rasterize fonts: {_FREETYPE_ERROR}")
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

    bitmap_buf = bytearray()

    if bpp == 1:
        # Monochrome accumulator: 1 bit at a time, 8 per byte
        _acc = [0, 7]  # [current_byte, current_shift]

        def enbit(v):
            if v:
                _acc[0] |= 1 << _acc[1]
            _acc[1] -= 1
            if _acc[1] < 0:
                bitmap_buf.append(_acc[0])
                _acc[0] = 0
                _acc[1] = 7

        def flush_acc():
            if _acc[1] != 7:
                bitmap_buf.append(_acc[0])
                _acc[0] = 0
                _acc[1] = 7

        def pad_bits(total_bits):
            tail = total_bits & 7
            if tail:
                for _ in range(8 - tail):
                    enbit(0)

        def glyph_byte_count(w, rows):
            return (w * rows + 7) // 8

    else:  # bpp == 2
        # 2bpp accumulator: 2 bits at a time, 4 pixels per byte
        _acc = [0, 6]  # [current_byte, current_shift]

        def enbit(v):
            # v is 0-3
            _acc[0] |= (v & 0x03) << _acc[1]
            _acc[1] -= 2
            if _acc[1] < 0:
                bitmap_buf.append(_acc[0])
                _acc[0] = 0
                _acc[1] = 6

        def flush_acc():
            if _acc[1] != 6:
                bitmap_buf.append(_acc[0])
                _acc[0] = 0
                _acc[1] = 6

        def pad_bits(total_pixels):
            tail = total_pixels & 3
            if tail:
                for _ in range(4 - tail):
                    enbit(0)

        def glyph_byte_count(w, rows):
            return (w * rows + 3) // 4

    table = []
    bmap_offset = 0

    load_target = FT_LOAD_TARGET_MONO if bpp == 1 else FT_LOAD_DEFAULT
    render_mode = FT_RENDER_MODE_MONO if bpp == 1 else FT_RENDER_MODE_NORMAL

    for code in codes:
        err = freetype.FT_Load_Char(face, code, load_target)
        if err:
            sys.stderr.write("Warning: error %d loading U+%04X\n" % (err, code))
            table.append((code, _zero_glyph(bmap_offset)))
            continue

        err = freetype.FT_Render_Glyph(face.contents.glyph, render_mode)
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

        total_pixels = w * rows

        if bpp == 1:
            # 1bpp: extract bits from monochrome bitmap
            for y in range(rows):
                for x in range(w):
                    bi = x // 8
                    bm = 0x80 >> (x & 7)
                    ao = y * pitch + bi
                    enbit(raw[ao] & bm if 0 <= ao < len(raw) else 0)
        else:
            # 2bpp: extract 8-bit gray, quantise to 2 bits
            for y in range(rows):
                row_start = y * pitch
                for x in range(w):
                    g = raw[row_start + x] if row_start + x < len(raw) else 0
                    enbit(_gray_to_2bpp(g))

        # Pad to byte boundary
        pad_bits(total_pixels)
        bmap_offset += glyph_byte_count(w, rows)
        freetype.FT_Done_Glyph(ft_glyph)

    flush_acc()

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

V3_MAGIC = b"\x00\x03"
V3_HEADER = 12
V3_VARIANTS_AT = 8   # bold, italic, bold italic font ids
GLYPH_V2 = 7
GLYPH_V3 = 14


def _check(code, name, v, lo, hi, hint):
    if not lo <= v <= hi:
        raise ValueError(f"U+{code:04X} {name}={v} out of range {lo}..{hi}{hint}")


def write_resource(bitmap, table, y_advance, font_id, bpp, out, version=None,
                   variants=(0, 0, 0)):
    """
    Write the binary font resource to `out` (binary-mode file or buffer).

    V1 (bpp=1):  4-byte header  | N*2 codepoints | N*7 glyphs  | bitmap
    V2 (bpp>=2): 7-byte header  | N*2 codepoints | N*7 glyphs  | bitmap
    V3 (version=3, any bpp):
                 12-byte header | N*2 codepoints | N*14 glyphs | bitmap
    `version` None = V1 or V2 by bpp. `variants`: V3's bold / italic / bold
    italic font ids.
    """
    v3 = version == 3
    n = len(table)
    hint = "" if v3 else ": too large for V1/V2; use V3 (--v3, ferrite-ui-xl devices)"
    if v3:
        if not 0 <= y_advance <= 0xFFFF:
            raise ValueError(f"y_advance={y_advance}px exceeds u16 max")
        out.write(V3_MAGIC + struct.pack("<HHBB3BB", n, y_advance, font_id, bpp, *variants, 0))
    else:
        if not 0 <= y_advance <= 255:
            raise ValueError(f"y_advance={y_advance}px exceeds u8 max (255){hint}")
        if bpp == 1:
            out.write(struct.pack("<HBB", n, y_advance, font_id))
        else:
            out.write(struct.pack("<BBHBBB", 0x00, 0x02, n, y_advance, font_id, bpp))

    for code, _g in table:
        out.write(struct.pack("<H", code))
    for code, g in table:
        bmo = g["bitmapOffset"]
        w, h, xa = g["width"], g["height"], g["xAdvance"]
        xo, yo = g["xOffset"], g["yOffset"]
        if v3:
            _check(code, "bitmapOffset", bmo, 0, 0xFFFFFFFF, "")
            for name, val in (("width", w), ("height", h), ("xAdvance", xa)):
                _check(code, name, val, 0, 0xFFFF, "")
            for name, val in (("xOffset", xo), ("yOffset", yo)):
                _check(code, name, val, -0x8000, 0x7FFF, "")
            out.write(struct.pack("<IHHHhh", bmo, w, h, xa, xo, yo))
        else:
            _check(code, "bitmapOffset", bmo, 0, 0xFFFF, hint)
            for name, val in (("width", w), ("height", h), ("xAdvance", xa)):
                _check(code, name, val, 0, 255, hint)
            for name, val in (("xOffset", xo), ("yOffset", yo)):
                _check(code, name, val, -128, 127, hint)
            out.write(struct.pack("<HBBBbb", bmo, w, h, xa, xo, yo))
    out.write(bitmap)


def parse_header(data):
    """A font resource's header: dict(version, num_glyphs, y_advance, font_id,
    font_id_at, bpp, hdr_len, glyph_bytes, variants). Same detection as
    ferrite-ui-xl's `font::Header::read`. Raises ValueError if too short."""
    data = bytes(data)
    if len(data) < 4:
        raise ValueError(f"font binary too small ({len(data)} bytes)")
    if data[:2] == V3_MAGIC and len(data) >= V3_HEADER and data[7] in (1, 2):
        n, y_adv = struct.unpack_from("<HH", data, 2)
        if len(data) >= V3_HEADER + n * (2 + GLYPH_V3):
            return dict(version=3, num_glyphs=n, y_advance=y_adv, font_id=data[6],
                        font_id_at=6, bpp=data[7], hdr_len=V3_HEADER, glyph_bytes=GLYPH_V3,
                        variants=tuple(data[8:11]))
    if data[:2] == b"\x00\x02" and len(data) >= 7:
        n = struct.unpack_from("<H", data, 2)[0]
        return dict(version=2, num_glyphs=n, y_advance=data[4], font_id=data[5],
                    font_id_at=5, bpp=data[6], hdr_len=7, glyph_bytes=GLYPH_V2,
                    variants=(0, 0, 0))
    n = struct.unpack_from("<H", data, 0)[0]
    return dict(version=1, num_glyphs=n, y_advance=data[2], font_id=data[3],
                font_id_at=3, bpp=1, hdr_len=4, glyph_bytes=GLYPH_V2,
                variants=(0, 0, 0))


def read_resource(data):
    """Decode a font resource: (header dict, [(code, glyph dict)], bitmap bytes)."""
    data = bytes(data)
    h = parse_header(data)
    n, rec = h["num_glyphs"], h["glyph_bytes"]
    cp_at = h["hdr_len"]
    gl_at = cp_at + 2 * n
    bm_at = gl_at + rec * n
    if len(data) < bm_at:
        raise ValueError(f"font binary truncated ({len(data)} < {bm_at} bytes)")
    fmt = "<IHHHhh" if rec == GLYPH_V3 else "<HBBBbb"
    table = []
    for i in range(n):
        code = struct.unpack_from("<H", data, cp_at + 2 * i)[0]
        bmo, w, hh, xa, xo, yo = struct.unpack_from(fmt, data, gl_at + rec * i)
        table.append((code, {"bitmapOffset": bmo, "width": w, "height": hh,
                             "xAdvance": xa, "xOffset": xo, "yOffset": yo}))
    return h, table, data[bm_at:]


def to_v3(data):
    """A V1/V2/V3 font resource as V3 (same glyphs and bitmap: same pixels)."""
    h, table, bitmap = read_resource(data)
    if h["version"] == 3:
        return bytes(data)
    buf = io.BytesIO()
    write_resource(bitmap, table, h["y_advance"], h["font_id"], h["bpp"], buf, version=3,
                   variants=h["variants"])
    return buf.getvalue()


# ── output: Rust source ───────────────────────────────────────────────────────

def write_code(bitmap, table, y_advance, bpp, out):
    """
    Write Rust source to `out` (text-mode file).

    Produces: Y_ADVANCE, BPP, CODEPOINTS, BITMAP, GLYPHS with fixed names.
    Intended use: save as src/embedded_font.rs (or similar), then
    call Font::from_embedded(&GLYPHS, &CODEPOINTS, &BITMAP, Y_ADVANCE, BPP).
    """
    n = len(table)
    m = len(bitmap)

    out.write("// Generated by ferrite_font_converter.py (code mode, bpp=%d).\n" % bpp)
    out.write("// %d glyphs, y_advance=%d, bitmap=%d bytes\n" % (n, y_advance, m))
    out.write("\n")
    out.write("use crate::font::GfxGlyph;\n")
    out.write("\n")
    out.write("pub const Y_ADVANCE: u8 = %d;\n" % y_advance)
    out.write("pub const BPP: u8 = %d;\n" % bpp)
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
    p.add_argument(
        "--bpp", type=int, choices=[1, 2], default=1,
        help="bits per pixel: 1 = monochrome (default), 2 = anti-aliased (4 gray levels)")
    p.add_argument(
        "--v3", action="store_true",
        help="resource: V3 layout (glyphs over 255 px, bitmaps over 64 KB); "
             "ferrite-ui-xl devices only (caps.font_v3)")


def _resolve_codes(args):
    if args.ranges:
        return parse_ranges(args.ranges)
    return list(range(0x20, 0x7F))   # printable ASCII default


def cmd_resource(args):
    if args.font_id == 0:
        sys.exit("error: font-id 0 is reserved for the embedded font "
                 "(load_by_id always skips it)")
    codes = _resolve_codes(args)
    bitmap, table, y_advance = rasterize(args.fontfile, args.size, codes, args.bpp)
    n, m = len(table), len(bitmap)
    buf = io.BytesIO()
    try:
        write_resource(bitmap, table, y_advance, args.font_id, args.bpp, buf,
                       3 if args.v3 else None)
    except ValueError as e:
        sys.exit(f"error: {e}")
    version = 3 if args.v3 else (1 if args.bpp == 1 else 2)
    sys.stderr.write("resource: %d glyphs, bpp=%d, V%d, bitmap %d B, total %d B\n"
                     % (n, args.bpp, version, m, len(buf.getvalue())))
    if args.output:
        with open(args.output, "wb") as f:
            f.write(buf.getvalue())
    else:
        sys.stdout.buffer.write(buf.getvalue())


def cmd_code(args):
    codes = _resolve_codes(args)
    bitmap, table, y_advance = rasterize(args.fontfile, args.size, codes, args.bpp)
    n, m = len(table), len(bitmap)
    sys.stderr.write("code: %d glyphs, bpp=%d, bitmap %d B\n" % (n, args.bpp, m))
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            write_code(bitmap, table, y_advance, args.bpp, f)
    else:
        write_code(bitmap, table, y_advance, args.bpp, sys.stdout)


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
            "Write a Rust .rs source with Y_ADVANCE, BPP, CODEPOINTS, BITMAP, GLYPHS "
            "arrays. Use with Font::from_embedded(&GLYPHS, &CODEPOINTS, &BITMAP, "
            "Y_ADVANCE, BPP)."
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
