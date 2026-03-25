#!/usr/bin/env python3
"""
Adafruit GFX font C header → Rust embedded_font.rs dönüştürücü.

Kullanım:
    python font2rust.py FreeMono9pt7b.h > ../src/embedded_font.rs

Çıktı: src/embedded_font.rs formatında Rust kaynak kodu.
"""

import re
import sys


def parse_header(path: str):
    with open(path, "r") as f:
        src = f.read()

    # Bitmap array
    bm = re.search(
        r"const\s+uint8_t\s+\w+Bitmaps\[\]\s+PROGMEM\s*=\s*\{([^}]+)\}", src
    )
    if not bm:
        raise ValueError("Bitmap array not found")
    bitmap_bytes = [int(x.strip(), 0) for x in bm.group(1).split(",") if x.strip()]

    # Glyph array
    gm = re.search(
        r"const\s+GFXglyph\s+\w+Glyphs\[\]\s+PROGMEM\s*=\s*\{(.+?)\};",
        src,
        re.DOTALL,
    )
    if not gm:
        raise ValueError("Glyph array not found")

    glyphs = []
    for m in re.finditer(r"\{([^}]+)\}", gm.group(1)):
        vals = [v.strip() for v in m.group(1).split(",")]
        nums = [int(v) for v in vals[:6]]
        glyphs.append(nums)

    # Font struct (first, last, yAdvance)
    fm = re.search(
        r"const\s+GFXfont\s+\w+\s+PROGMEM\s*=\s*\{[^,]+,[^,]+,\s*(0x[\dA-Fa-f]+|\d+)\s*,\s*(0x[\dA-Fa-f]+|\d+)\s*,\s*(\d+)\s*\}",
        src,
    )
    if not fm:
        raise ValueError("Font struct not found")
    first = int(fm.group(1), 0)
    last = int(fm.group(2), 0)
    y_advance = int(fm.group(3))

    return bitmap_bytes, glyphs, first, last, y_advance


def emit_rust(bitmap_bytes, glyphs, first, last, y_advance):
    lines = []
    lines.append(
        "/// Gömülü font — tools/font2rust.py ile C header'dan dönüştürüldü."
    )
    lines.append("")
    lines.append("use crate::font::GfxGlyph;")
    lines.append("")
    lines.append(f"pub const FIRST: u16 = 0x{first:02X};")
    lines.append(f"pub const LAST: u16 = 0x{last:02X};")
    lines.append(f"pub const Y_ADVANCE: u8 = {y_advance};")
    lines.append("")

    # Bitmap
    lines.append(f"pub static BITMAP: [u8; {len(bitmap_bytes)}] = [")
    for i in range(0, len(bitmap_bytes), 12):
        chunk = bitmap_bytes[i : i + 12]
        hex_vals = ", ".join(f"0x{b:02X}" for b in chunk)
        lines.append(f"    {hex_vals},")
    lines.append("];")
    lines.append("")

    # Glyphs
    lines.append(f"pub static GLYPHS: [GfxGlyph; {len(glyphs)}] = [")
    for i, g in enumerate(glyphs):
        offset, w, h, x_adv, x_off, y_off = g
        ch = chr(first + i) if 0x20 <= first + i <= 0x7E else "?"
        ch_repr = repr(ch) if ch != "\\" else "'\\\\'"
        lines.append(
            f"    GfxGlyph::new({offset:3}, {w:2}, {h:2}, {x_adv:2}, {x_off:3}, {y_off:3}), "
            f"// 0x{first + i:02X} {ch_repr}"
        )
    lines.append("];")
    lines.append("")

    return "\n".join(lines)


def main():
    if len(sys.argv) < 2:
        print(f"Kullanım: {sys.argv[0]} <header.h>", file=sys.stderr)
        sys.exit(1)

    bitmap, glyphs, first, last, y_adv = parse_header(sys.argv[1])
    print(emit_rust(bitmap, glyphs, first, last, y_adv))


if __name__ == "__main__":
    main()
