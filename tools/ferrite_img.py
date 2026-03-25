#!/usr/bin/env python3
"""ferrite_img.py — PNG → Ferrite Image (.fi) converter.

Ferrite Image (FI) format:
  Header (8 byte):
    [0..2] magic: u16 LE = 0x4649 ("FI")
    [2..4] width: u16 LE
    [4..6] height: u16 LE
    [6]    flags: u8  (bit 0-1: mode)
    [7]    colors: u8 (palette count, indexed modda, 0=256)
  Indexed modda: palette — colors × 2 byte (RGB565 LE)
  Piksel verisi: mode'a göre raw / RLE / indexed+RLE

Modlar:
  0 = Raw RGB565
  1 = RLE RGB565 (PackBits)
  2 = Indexed + RLE (palette + PackBits)

RLE encoding (PackBits varyantı):
  0x00..0x7F: literal run — sonraki (n+1) birim olduğu gibi
  0x80..0xFF: repeat run — sonraki birim (n−126) kez tekrar [2..129]

Usage:
  python ferrite_img.py input.png output.fi [--mode auto|raw|rle|indexed]
  python ferrite_img.py info image.fi
  python ferrite_img.py input.png output.fi --max-colors 64
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


def rle_encode_u16(data: list[int]) -> bytearray:
    """PackBits RLE for RGB565 (2 byte units)."""
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
            out.extend(struct.pack("<H", val))
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
                out.extend(struct.pack("<H", data[j]))

    return out


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

def encode_raw(pixels: list[int]) -> bytearray:
    """Raw RGB565 encode."""
    out = bytearray()
    for c in pixels:
        out.extend(struct.pack("<H", c))
    return out


def encode_rle(pixels: list[int]) -> bytearray:
    """RLE RGB565 encode."""
    return rle_encode_u16(pixels)


def encode_indexed_rle(pixels: list[int], palette: list[int]) -> tuple[bytearray, bytearray]:
    """Indexed + RLE encode. Returns (palette_bytes, rle_data)."""
    # Palette lookup
    pal_map = {c: i for i, c in enumerate(palette)}

    # Pikselleri index'e çevir
    indices = [pal_map.get(c, 0) for c in pixels]

    # RLE encode (u8 indices)
    rle_data = rle_encode(indices)

    # Palette binary
    pal_bytes = bytearray()
    for c in palette:
        pal_bytes.extend(struct.pack("<H", c))

    return pal_bytes, rle_data


def build_fi(
    width: int,
    height: int,
    pixels: list[int],
    mode: int,
    palette: list[int] | None = None,
) -> bytearray:
    """FI binary oluştur."""
    out = bytearray()

    # Header
    flags = mode & 0x03
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
        pal_bytes, rle_data = encode_indexed_rle(pixels, palette)
        out.extend(pal_bytes)
        out.extend(rle_data)
    elif mode == MODE_RLE:
        out.extend(encode_rle(pixels))
    else:
        out.extend(encode_raw(pixels))

    return out


# --- Main converter ---

def convert(
    input_path: str,
    output_path: str,
    mode: str = "auto",
    max_colors: int = 256,
) -> dict:
    """PNG → FI dönüşümü."""
    img = Image.open(input_path)
    width, height = img.size

    pixels, unique = analyze_image(img)
    num_colors = len(unique)

    # Mode seçimi
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

    # Indexed modda renk sayısı kontrolü
    palette = None
    if chosen_mode == MODE_INDEXED_RLE:
        if num_colors > 256:
            # Quantize: Pillow ile renk azaltma
            img_q = img.convert("RGB").quantize(colors=max_colors, method=Image.Quantize.MEDIANCUT)
            img_rgb = img_q.convert("RGB")
            pixels, unique = analyze_image(img_rgb)
            num_colors = len(unique)
        palette = unique[:256]

    # Encode
    raw_size = width * height * 2
    fi_data = build_fi(width, height, pixels, chosen_mode, palette)

    # Write
    Path(output_path).write_bytes(fi_data)

    stats = {
        "width": width,
        "height": height,
        "mode": MODE_NAMES[chosen_mode],
        "colors": num_colors,
        "raw_size": raw_size,
        "fi_size": len(fi_data),
        "ratio": len(fi_data) / raw_size * 100 if raw_size > 0 else 0,
    }
    return stats


# --- Info command ---

def info(path: str):
    """FI dosyasının bilgilerini göster."""
    data = Path(path).read_bytes()
    if len(data) < 8:
        print("Error: File too small")
        return

    magic, width, height, flags, colors = struct.unpack_from("<HHHBB", data)
    if magic != MAGIC:
        print(f"Error: Bad magic 0x{magic:04X} (expected 0x{MAGIC:04X})")
        return

    mode = flags & 0x03
    pal_count = 256 if colors == 0 and mode == MODE_INDEXED_RLE else colors

    print(f"Ferrite Image: {width}×{height}")
    print(f"Mode: {MODE_NAMES.get(mode, f'unknown({mode})')}")
    if mode == MODE_INDEXED_RLE:
        print(f"Palette: {pal_count} colors")
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

    i = 2
    while i < len(args):
        if args[i] == "--mode" and i + 1 < len(args):
            mode = args[i + 1]
            i += 2
        elif args[i] == "--max-colors" and i + 1 < len(args):
            max_colors = int(args[i + 1])
            i += 2
        else:
            print(f"Unknown option: {args[i]}")
            return

    stats = convert(input_path, output_path, mode=mode, max_colors=max_colors)

    print(f"Converted: {input_path} → {output_path}")
    print(f"  Size: {stats['width']}×{stats['height']}")
    print(f"  Mode: {stats['mode']} ({stats['colors']} unique colors)")
    print(f"  Raw:  {stats['raw_size']} bytes")
    print(f"  FI:   {stats['fi_size']} bytes ({stats['ratio']:.1f}%)")


if __name__ == "__main__":
    main()
