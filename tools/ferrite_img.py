#!/usr/bin/env python3
"""ferrite_img.py — PNG → Ferrite Image (.fi) converter.

Ferrite Image (FI) format:
  Header (8 bytes):
    [0..2] magic: u16 LE = 0x4649 ("FI")
    [2..4] width: u16 LE
    [4..6] height: u16 LE
    [6]    flags: u8  (bit 0-1: mode, bit 2: has_alpha)
    [7]    colors: u8 (palette count in indexed mode, 0=256)
  Indexed mode: palette — colors × 2 bytes (RGB565 LE),
                or colors × 3 bytes (RGB565 LE + alpha u8) when has_alpha
  Pixel data: raw / RLE / indexed+RLE depending on mode

Modes:
  0 = Raw RGB565
  1 = RLE RGB565 (PackBits)
  2 = Indexed + RLE (palette + PackBits)

Alpha (flags bit 2, only for devices with caps.image_alpha):
  Pixel units widen from 2 to 3 bytes (RGB565 LE + alpha u8) in raw/RLE
  modes; in indexed mode the palette entries widen instead and the index
  stream is unchanged. A repeat run requires color AND alpha to match.

RLE encoding (PackBits variant):
  0x00..0x7F: literal run — next (n+1) units verbatim
  0x80..0xFF: repeat run — next unit repeated (n−126) times [2..129]

Usage:
  python ferrite_img.py input.png output.fi [--mode auto|raw|rle|indexed]
  python ferrite_img.py info image.fi
  python ferrite_img.py input.png output.fi --max-colors 64
  python ferrite_img.py input.png output.fi --alpha
"""

import struct
import sys
from pathlib import Path

try:
    from PIL import Image
except ImportError:
    print("Error: Pillow required — pip install Pillow", file=sys.stderr)
    sys.exit(1)

# --- Constants ---

MAGIC = 0x4649
MODE_RAW = 0
MODE_RLE = 1
MODE_INDEXED_RLE = 2
FLAG_ALPHA = 0x04  # flags bit 2 — pixel/palette units carry an alpha byte

MODE_NAMES = {MODE_RAW: "raw", MODE_RLE: "rle", MODE_INDEXED_RLE: "indexed_rle"}


# --- RGB565 conversion ---

def rgb_to_565(r: int, g: int, b: int) -> int:
    """RGB888 → RGB565."""
    return ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)


def rgb565_to_bytes(c: int) -> bytes:
    """RGB565 → 2 byte LE."""
    return struct.pack("<H", c)


# --- RLE encoder ---

def rle_encode(data: list[int]) -> bytearray:
    """PackBits-style RLE encode.

    Input: flat list of values (u8 indices or u16 packed as single ints).
    Output: bytearray of control bytes + data.

    unit_size is handled by caller — this operates on abstract "units".
    Each unit in data is an int that will be serialized by the caller.
    """
    out = bytearray()
    n = len(data)
    i = 0

    while i < n:
        # Check for repeat run (≥2 identical values)
        if i + 1 < n and data[i] == data[i + 1]:
            val = data[i]
            run = 1
            while i + run < n and data[i + run] == val and run < 129:
                run += 1
            # Repeat: ctrl = run + 126
            out.append(run + 126)
            out.append(val)
            i += run
        else:
            # Literal run: collect until we hit a repeat of ≥2
            lit_start = i
            while i < n:
                if i + 1 < n and data[i] == data[i + 1]:
                    break
                i += 1
                if i - lit_start >= 128:
                    break
            lit_count = i - lit_start
            out.append(lit_count - 1)  # ctrl = count - 1
            out.extend(data[lit_start:i])

    return out


def pack_unit(unit: int, has_alpha: bool) -> bytes:
    """Serialize one pixel/palette unit: RGB565 LE, plus alpha byte when
    has_alpha (units carry the alpha in bits 16-23)."""
    if has_alpha:
        return struct.pack("<H", unit & 0xFFFF) + bytes([(unit >> 16) & 0xFF])
    return struct.pack("<H", unit)


def rle_encode_units(data: list[int], has_alpha: bool) -> bytearray:
    """PackBits RLE over pixel units (2 or 3 bytes each). A repeat run
    requires the full unit to match — color AND alpha."""
    out = bytearray()
    n = len(data)
    i = 0

    while i < n:
        if i + 1 < n and data[i] == data[i + 1]:
            val = data[i]
            run = 1
            while i + run < n and data[i + run] == val and run < 129:
                run += 1
            out.append(run + 126)
            out.extend(pack_unit(val, has_alpha))
            i += run
        else:
            lit_start = i
            while i < n:
                if i + 1 < n and data[i] == data[i + 1]:
                    break
                i += 1
                if i - lit_start >= 128:
                    break
            lit_count = i - lit_start
            out.append(lit_count - 1)
            for j in range(lit_start, lit_start + lit_count):
                out.extend(pack_unit(data[j], has_alpha))

    return out


def rle_encode_u16(data: list[int]) -> bytearray:
    """PackBits RLE for RGB565 (2 byte units)."""
    return rle_encode_units(data, has_alpha=False)


# --- Image analysis ---

def analyze_image(img: Image.Image) -> tuple[list[int], list[tuple[int, int, int]]]:
    """Resmi analiz et. RGB565 piksel listesi ve unique renkleri döndür."""
    img = img.convert("RGB")
    pixels_rgb = list(img.getdata())

    # RGB565'e çevir
    pixels_565 = [rgb_to_565(r, g, b) for r, g, b in pixels_rgb]

    # Unique renkler (sıralı, ilk görülme)
    seen = {}
    for c in pixels_565:
        if c not in seen:
            seen[c] = len(seen)

    unique = list(seen.keys())
    return pixels_565, unique


# --- Encoder ---

def encode_raw(pixels: list[int], has_alpha: bool = False) -> bytearray:
    """Raw encode — one 2- or 3-byte unit per pixel."""
    out = bytearray()
    for c in pixels:
        out.extend(pack_unit(c, has_alpha))
    return out


def encode_rle(pixels: list[int], has_alpha: bool = False) -> bytearray:
    """RLE encode over pixel units."""
    return rle_encode_units(pixels, has_alpha)


def encode_indexed_rle(
    pixels: list[int],
    palette: list[int],
    has_alpha: bool = False,
) -> tuple[bytearray, bytearray]:
    """Indexed + RLE encode. Returns (palette_bytes, rle_data).

    With alpha, palette entries are (color, alpha) units (3 bytes each);
    the index stream itself is unchanged.
    """
    # Palette lookup
    pal_map = {c: i for i, c in enumerate(palette)}

    # Map pixels to palette indices
    indices = [pal_map.get(c, 0) for c in pixels]

    # RLE encode (u8 indices)
    rle_data = rle_encode(indices)

    # Palette binary
    pal_bytes = bytearray()
    for c in palette:
        pal_bytes.extend(pack_unit(c, has_alpha))

    return pal_bytes, rle_data


def build_fi(
    width: int,
    height: int,
    pixels: list[int],
    mode: int,
    palette: list[int] | None = None,
    has_alpha: bool = False,
) -> bytearray:
    """Build the FI binary."""
    out = bytearray()

    # Header
    flags = (mode & 0x03) | (FLAG_ALPHA if has_alpha else 0)
    colors = len(palette) if palette and mode == MODE_INDEXED_RLE else 0
    if colors == 256:
        colors = 0  # 0 means 256

    out.extend(struct.pack("<H", MAGIC))
    out.extend(struct.pack("<H", width))
    out.extend(struct.pack("<H", height))
    out.append(flags)
    out.append(colors & 0xFF)

    # Palette (indexed mode only)
    if mode == MODE_INDEXED_RLE and palette:
        pal_bytes, rle_data = encode_indexed_rle(pixels, palette, has_alpha)
        out.extend(pal_bytes)
        out.extend(rle_data)
    elif mode == MODE_RLE:
        out.extend(encode_rle(pixels, has_alpha))
    else:
        out.extend(encode_raw(pixels, has_alpha))

    return out


# --- Main converter ---

def _ordered_unique(values: list[int]) -> list[int]:
    """Unique values in first-seen order."""
    seen = {}
    for v in values:
        if v not in seen:
            seen[v] = len(seen)
    return list(seen.keys())


def convert_bytes(
    img: Image.Image,
    mode: str = "auto",
    max_colors: int = 256,
    want_alpha: bool = False,
) -> tuple[bytes, dict]:
    """Convert a PIL image to FI binary. Shared core for the CLI and
    ferrite_build.

    `want_alpha` opts the image into FI alpha encoding (gate it on the
    target device's caps.image_alpha); the alpha plane is only emitted
    when the source actually contains non-opaque pixels.
    """
    width, height = img.size

    # Pixel units: rgb565, or rgb565 | (alpha << 16) when alpha is active.
    has_alpha = False
    alphas = None
    if want_alpha:
        rgba = img.convert("RGBA")
        alphas = [p[3] for p in rgba.getdata()]
        has_alpha = any(a < 255 for a in alphas)

    if has_alpha:
        pixels = [
            rgb_to_565(r, g, b) | (a << 16) for r, g, b, a in rgba.getdata()
        ]
        unique = _ordered_unique(pixels)
    else:
        pixels, unique = analyze_image(img)
    num_colors = len(unique)

    # Mode selection
    if mode == "auto":
        if num_colors <= max_colors:
            chosen_mode = MODE_INDEXED_RLE
        else:
            chosen_mode = MODE_RLE
    elif mode == "raw":
        chosen_mode = MODE_RAW
    elif mode == "rle":
        chosen_mode = MODE_RLE
    elif mode == "indexed":
        chosen_mode = MODE_INDEXED_RLE
    else:
        raise ValueError(f"Unknown mode: {mode}")

    # Indexed mode: quantize when the palette would overflow
    palette = None
    if chosen_mode == MODE_INDEXED_RLE:
        if num_colors > 256:
            if has_alpha:
                # Quantize the RGB channels, re-attach each pixel's alpha.
                img_q = rgba.convert("RGB").quantize(
                    colors=max_colors, method=Image.Quantize.MEDIANCUT
                ).convert("RGB")
                q565 = [rgb_to_565(r, g, b) for r, g, b in img_q.getdata()]
                pixels = [c | (a << 16) for c, a in zip(q565, alphas)]
                unique = _ordered_unique(pixels)
                if len(unique) > 256:
                    # Too many (color, alpha) pairs — quantize alpha too.
                    # MEDIANCUT rejects RGBA; FASTOCTREE handles it.
                    img_q = rgba.quantize(
                        colors=max_colors, method=Image.Quantize.FASTOCTREE
                    ).convert("RGBA")
                    pixels = [
                        rgb_to_565(r, g, b) | (a << 16)
                        for r, g, b, a in img_q.getdata()
                    ]
                    unique = _ordered_unique(pixels)
            else:
                img_q = img.convert("RGB").quantize(
                    colors=max_colors, method=Image.Quantize.MEDIANCUT
                )
                img_rgb = img_q.convert("RGB")
                pixels, unique = analyze_image(img_rgb)
            num_colors = len(unique)
        palette = unique[:256]

    # Encode
    raw_size = width * height * 2
    fi_data = build_fi(width, height, pixels, chosen_mode, palette, has_alpha)

    stats = {
        "width": width,
        "height": height,
        "mode": MODE_NAMES[chosen_mode],
        "colors": num_colors,
        "alpha": has_alpha,
        "raw_size": raw_size,
        "fi_size": len(fi_data),
        "ratio": len(fi_data) / raw_size * 100 if raw_size > 0 else 0,
    }
    return bytes(fi_data), stats


def convert(
    input_path: str,
    output_path: str,
    mode: str = "auto",
    max_colors: int = 256,
    want_alpha: bool = False,
) -> dict:
    """PNG → FI file conversion."""
    img = Image.open(input_path)
    fi_data, stats = convert_bytes(
        img, mode=mode, max_colors=max_colors, want_alpha=want_alpha
    )
    Path(output_path).write_bytes(fi_data)
    return stats


# --- Info command ---

def info(path: str):
    """Print info about an FI file."""
    data = Path(path).read_bytes()
    if len(data) < 8:
        print("Error: File too small")
        return

    magic, width, height, flags, colors = struct.unpack_from("<HHHBB", data)
    if magic != MAGIC:
        print(f"Error: Bad magic 0x{magic:04X} (expected 0x{MAGIC:04X})")
        return

    mode = flags & 0x03
    has_alpha = bool(flags & FLAG_ALPHA)
    pal_count = 256 if colors == 0 and mode == MODE_INDEXED_RLE else colors

    print(f"Ferrite Image: {width}×{height}")
    print(f"Mode: {MODE_NAMES.get(mode, f'unknown({mode})')}")
    print(f"Alpha: {'yes' if has_alpha else 'no'}")
    if mode == MODE_INDEXED_RLE:
        entry = 3 if has_alpha else 2
        print(f"Palette: {pal_count} colors ({entry} bytes/entry)")
    print(f"File size: {len(data)} bytes")
    print(f"Raw equiv: {width * height * 2} bytes")
    if width * height > 0:
        print(f"Ratio: {len(data) / (width * height * 2) * 100:.1f}%")


# --- CLI ---

def main():
    args = sys.argv[1:]

    if not args or args[0] in ("-h", "--help"):
        print(__doc__)
        return

    if args[0] == "info":
        if len(args) < 2:
            print("Usage: ferrite_img.py info <file.fi>")
            return
        info(args[1])
        return

    # Convert mode
    if len(args) < 2:
        print("Usage: ferrite_img.py <input.png> <output.fi> [options]")
        return

    input_path = args[0]
    output_path = args[1]

    mode = "auto"
    max_colors = 256
    want_alpha = False

    i = 2
    while i < len(args):
        if args[i] == "--mode" and i + 1 < len(args):
            mode = args[i + 1]
            i += 2
        elif args[i] == "--max-colors" and i + 1 < len(args):
            max_colors = int(args[i + 1])
            i += 2
        elif args[i] == "--alpha":
            want_alpha = True
            i += 1
        else:
            print(f"Unknown option: {args[i]}")
            return

    stats = convert(input_path, output_path, mode=mode, max_colors=max_colors,
                    want_alpha=want_alpha)

    print(f"Converted: {input_path} → {output_path}")
    print(f"  Size: {stats['width']}×{stats['height']}")
    print(f"  Mode: {stats['mode']} ({stats['colors']} unique colors"
          f"{', alpha' if stats['alpha'] else ''})")
    print(f"  Raw:  {stats['raw_size']} bytes")
    print(f"  FI:   {stats['fi_size']} bytes ({stats['ratio']:.1f}%)")


if __name__ == "__main__":
    main()
