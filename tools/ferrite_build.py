#!/usr/bin/env python3
"""ferrite_build.py — JSON proje dosyasından flash filesystem binary'si oluşturur.

Flash FS layout (W25Q256):
  0x000000 - 0x000FFF : Reserved (4KB)
  0x001000 - 0x00100F : Header (16B: magic + version + screen + count + checksum)
  0x001010 - 0x001FFF : Resource Table (max 127 × 32B)
  0x002000+            : Resource data (packed)

Usage:
  python ferrite_build.py project.json -o flash.bin
  python ferrite_build.py project.json --info

JSON format:
  {
    "version": 1,
    "screen": [800, 480],
    "fonts": [
      {"name": "sans12", "font_id": 1, "header": "assets/sans12.bin", "data": "assets/sans12_bmp.bin"}
    ],
    "images": [
      {"name": "logo", "image_id": 1, "source": "assets/logo.png", "mode": "auto", "max_colors": 256}
    ],
    "pages": [
      {
        "name": "main",
        "background": "0x0000",
        "widgets": [
          {
            "type": "button",
            "location": [150, 100],
            "size": [200, 60],
            "background_color": "0xF800",
            "border": [1, 1, 1, 1],
            "border_color": "0xFFFF",
            "press_color": "0x07E0",
            "clickable": true,
            "padding": [10, 10, 10, 10],
            "children": [
              {
                "type": "label",
                "size": [180, 40],
                "text": "Click Me",
                "font_id": 0,
                "text_color": "0xFFFF",
                "text_align": "center"
              }
            ]
          }
        ]
      }
    ],
    "programs": [
      {"name": "init", "source": "scripts/init.fl", "exec_mode": "ram|flash"}
    ],
    "files": [
      {"name": "config", "source": "data/config.bin"}
    ]
  }
"""

import json
import struct
import sys
from pathlib import Path

# ferrite_cc ve ferrite_img'den import
sys.path.insert(0, str(Path(__file__).parent))
from ferrite_cc import Asm, Compiler, Prop, encode_svarint

# --- Constants (fs.rs ile birebir aynı) ---

MAGIC = b"FERR"
FS_BASE = 0x1000       # Sector 0 reserved for ConfigStore
HEADER_OFFSET = 0x0000  # Relative to FS image start (written at FS_BASE in flash)
HEADER_SIZE = 16
TABLE_OFFSET = HEADER_OFFSET + HEADER_SIZE  # 0x10
ENTRY_SIZE = 32
MAX_RESOURCES = 1000

RES_FONT = 0
RES_IMAGE = 1
RES_PROGRAM = 2
RES_PAGE = 3
RES_FILE = 4

# Resource flags (stored in entry reserved[0], byte 28)
RES_FLAG_FLASH_EXEC = 0x01

KIND_BASE = 0
KIND_LABEL = 1
KIND_BUTTON = 2

ALIGN_MAP = {"left": 0, "center": 1, "right": 2}


# --- Color parsing ---

def parse_color(val) -> int:
    """Parse color: int, "0xFFFF", "#F800", "rgb(255,0,0)"."""
    if isinstance(val, int):
        return val & 0xFFFF
    s = str(val).strip()
    if s.startswith("0x") or s.startswith("0X"):
        return int(s, 16) & 0xFFFF
    if s.startswith("#"):
        hex_str = s[1:]
        if len(hex_str) == 4:
            # #RGBS short RGB565 hex
            return int(hex_str, 16) & 0xFFFF
        elif len(hex_str) == 6:
            # #RRGGBB → RGB565
            r = int(hex_str[0:2], 16)
            g = int(hex_str[2:4], 16)
            b = int(hex_str[4:6], 16)
            return ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)
    if s.startswith("rgb("):
        parts = s[4:-1].split(",")
        r, g, b = int(parts[0]), int(parts[1]), int(parts[2])
        return ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)
    return int(s) & 0xFFFF


# --- Image conversion (ferrite_img.py logic inlined) ---

def convert_image(source_path: str, mode: str = "auto", max_colors: int = 256) -> bytes:
    """PNG → FI binary."""
    try:
        from PIL import Image
    except ImportError:
        print("Error: Pillow required — pip install Pillow", file=sys.stderr)
        sys.exit(1)

    FI_MAGIC = 0x4649
    MODE_RAW = 0
    MODE_RLE = 1
    MODE_INDEXED_RLE = 2

    img = Image.open(source_path).convert("RGB")
    width, height = img.size
    pixels_rgb = list(img.getdata())
    pixels_565 = [((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3) for r, g, b in pixels_rgb]

    # Unique colors
    seen = {}
    for c in pixels_565:
        if c not in seen:
            seen[c] = len(seen)
    unique = list(seen.keys())
    num_colors = len(unique)

    # Mode selection
    if mode == "auto":
        chosen = MODE_INDEXED_RLE if num_colors <= max_colors else MODE_RLE
    elif mode == "raw":
        chosen = MODE_RAW
    elif mode == "rle":
        chosen = MODE_RLE
    elif mode == "indexed":
        chosen = MODE_INDEXED_RLE
    else:
        chosen = MODE_RLE

    # Quantize if needed
    palette = None
    if chosen == MODE_INDEXED_RLE:
        if num_colors > 256:
            img_q = img.quantize(colors=max_colors, method=Image.Quantize.MEDIANCUT)
            img_rgb = img_q.convert("RGB")
            pixels_rgb = list(img_rgb.getdata())
            pixels_565 = [((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3) for r, g, b in pixels_rgb]
            seen = {}
            for c in pixels_565:
                if c not in seen:
                    seen[c] = len(seen)
            unique = list(seen.keys())
        palette = unique[:256]

    # Header
    colors_byte = len(palette) if palette else 0
    if colors_byte == 256:
        colors_byte = 0
    out = bytearray(struct.pack("<HHHBB", FI_MAGIC, width, height, chosen & 0x03, colors_byte))

    # Encode
    if chosen == MODE_INDEXED_RLE and palette:
        pal_map = {c: i for i, c in enumerate(palette)}
        indices = [pal_map.get(c, 0) for c in pixels_565]
        for c in palette:
            out.extend(struct.pack("<H", c))
        out.extend(_rle_encode_u8(indices))
    elif chosen == MODE_RLE:
        out.extend(_rle_encode_u16(pixels_565))
    else:
        for c in pixels_565:
            out.extend(struct.pack("<H", c))

    return bytes(out)


def _rle_encode_u8(data):
    """PackBits RLE for u8 values."""
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
            out.append(val)
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
            out.extend(data[lit_start:i])
    return out


def _rle_encode_u16(data):
    """PackBits RLE for RGB565 (2-byte units)."""
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


# --- Page: widget tree JSON → bytecode ---

def compile_page(page_json: dict, page_widget_id: int, base_id: int) -> tuple[bytes, int]:
    """Page JSON → (page_binary, content_widget_count).

    page_binary = bg_color(u16 LE) + bytecode
    content_widget_count = kaç widget bytecode ile oluşturuldu
    """
    bg_color = parse_color(page_json.get("background", "0x0000"))

    cc = Compiler(base_id=base_id)
    widgets = page_json.get("widgets", [])
    count = _compile_widgets(cc, widgets, page_widget_id)
    cc.halt()

    data = cc.build_page(bg_color)
    return data, count


def _compile_widgets(cc: Compiler, widgets: list, parent_id: int) -> int:
    """Widget listesini recursive olarak bytecode'a çevir. Returns widget count."""
    count = 0
    for w in widgets:
        name = f"_w{cc._next_id}"
        wid = cc.alloc(name)
        count += 1

        cc.target(name)

        # Type
        wtype = w.get("type", "base")
        if wtype == "label":
            cc.set_prop("kind", KIND_LABEL)
        elif wtype == "button":
            cc.set_prop("kind", KIND_BUTTON)

        # Location
        if "location" in w:
            loc = w["location"]
            cc.set_prop("location", loc[0], loc[1])

        # Size
        if "size" in w:
            sz = w["size"]
            cc.set_prop("size", sz[0], sz[1])

        # Box model
        if "margin" in w:
            m = w["margin"]
            if isinstance(m, list):
                cc.set_prop("margin", *m)
            else:
                cc.set_prop("margin", m, m, m, m)

        if "border" in w:
            b = w["border"]
            if isinstance(b, list):
                cc.set_prop("border", *b)
            else:
                cc.set_prop("border", b, b, b, b)

        if "padding" in w:
            p = w["padding"]
            if isinstance(p, list):
                cc.set_prop("padding", *p)
            else:
                cc.set_prop("padding", p, p, p, p)

        # Colors
        if "background_color" in w:
            cc.set_prop("bg_color", parse_color(w["background_color"]))
        if "border_color" in w:
            cc.set_prop("border_color", parse_color(w["border_color"]))
        if "text_color" in w:
            cc.set_prop("text_color", parse_color(w["text_color"]))
        if "press_color" in w:
            cc.set_prop("press_color", parse_color(w["press_color"]))
        if "gradient_color" in w:
            cc.set_prop("gradient_color", parse_color(w["gradient_color"]))
        if "gradient_dir" in w:
            cc.set_prop("gradient_dir", int(w["gradient_dir"]))

        # Flags
        if w.get("clickable", False):
            cc.set_prop("clickable", 1)
        if w.get("visible") is False:
            cc.set_prop("visible", 0)

        # Background image
        if "image_id" in w:
            cc.set_prop("image_id", w["image_id"])

        # Callbacks
        if "on_click" in w:
            cc.set_prop("on_click", w["on_click"])

        # Label properties
        if "font_id" in w:
            cc.set_prop("font_id", w["font_id"])
        if "text_align" in w:
            align = w["text_align"]
            cc.set_prop("text_align", ALIGN_MAP.get(align, 0) if isinstance(align, str) else align)
        if "text" in w:
            cc.set_text(w["text"])

        # Parent
        cc.parent(parent_id)

        # Children (recursive)
        children = w.get("children", [])
        if children:
            child_count = _compile_widgets(cc, children, wid)
            count += child_count

    return count


# --- Program compilation ---

def compile_program(source_path: str, include_dirs=None, render_mode="dirty") -> bytes:
    """Ferrite lang source → VM image (header + opcodes)."""
    try:
        from ferrite_lang import build_image, CompileError
    except ImportError:
        print(f"Warning: ferrite_lang not available, reading {source_path} as raw binary",
              file=sys.stderr)
        return Path(source_path).read_bytes()

    source = Path(source_path).read_text(encoding="utf-8")
    try:
        return build_image(source, filename=source_path, include_dirs=include_dirs,
                           render_mode=render_mode)
    except CompileError as e:
        raise CompileError(f"{source_path}: {e}") from e


# --- Flash filesystem builder ---

def build_fs(project: dict, resources: list[tuple[str, int, bytes, int]]) -> bytes:
    """Flash filesystem binary oluştur.

    resources: [(name, kind, data, flags), ...]
    """
    if len(resources) > MAX_RESOURCES:
        raise ValueError(f"Too many resources: {len(resources)} (max {MAX_RESOURCES})")

    screen = project.get("screen", [800, 480])
    version = project.get("version", 2)
    count = len(resources)

    # Data starts right after the resource table (relative to image start)
    data_base_rel = TABLE_OFFSET + count * ENTRY_SIZE

    # Resource table and data blob
    table = bytearray()
    data_blob = bytearray()

    for name, kind, data, flags in resources:
        # Name: 16 byte null-padded ASCII
        name_bytes = name.encode("ascii")[:15]
        name_padded = name_bytes + b'\x00' * (16 - len(name_bytes))

        # Offset: absolute flash address (FS image is written at FS_BASE)
        offset = FS_BASE + data_base_rel + len(data_blob)

        # Entry: name[16] + kind(1) + pad(3) + offset(4) + size(4) + reserved(4)
        entry = bytearray(ENTRY_SIZE)
        entry[0:16] = name_padded
        entry[16] = kind
        # [17:20] = padding (zeros)
        struct.pack_into("<I", entry, 20, offset)
        struct.pack_into("<I", entry, 24, len(data))
        entry[28] = flags  # resource flags (e.g. RES_FLAG_FLASH_EXEC)

        table.extend(entry)
        data_blob.extend(data)

    # Checksum: additive sum of table bytes
    checksum = sum(table) & 0xFFFFFFFF

    # Header (16 bytes)
    header = bytearray(HEADER_SIZE)
    header[0:4] = MAGIC
    struct.pack_into("<H", header, 4, version)
    struct.pack_into("<H", header, 6, screen[0])
    struct.pack_into("<H", header, 8, screen[1])
    struct.pack_into("<H", header, 10, count)
    struct.pack_into("<I", header, 12, checksum)

    # Full binary — header + table + data, no gaps
    binary = bytearray()
    binary.extend(header)
    binary.extend(table)
    binary.extend(data_blob)

    return bytes(binary)


# --- Main build pipeline ---

def build(project_path: str, output_path: str) -> dict:
    """JSON proje → flash binary."""
    project_dir = Path(project_path).parent
    project = json.loads(Path(project_path).read_text(encoding="utf-8"))

    # Resolve include directories relative to project dir
    include_dirs = [str(project_dir / d) for d in project.get("include_dirs", [])]

    resources = []  # (name, kind, data)

    # 1. Fonts — combined resource: header + bitmap data in one RES_FONT
    #    Flash layout: first(2) + last(2) + y_advance(1) + font_id(1) + glyphs + bitmap
    font_ids_seen = set()
    for font in project.get("fonts", []):
        name = font["name"]
        font_id = font.get("font_id")
        if font_id is None:
            print(f"Error: font '{name}' missing required 'font_id' field", file=sys.stderr)
            sys.exit(1)
        if font_id == 0:
            print(f"Error: font '{name}' has font_id=0 (reserved for embedded font)", file=sys.stderr)
            sys.exit(1)
        if font_id < 0 or font_id > 254:
            print(f"Error: font '{name}' font_id must be 1-254, got {font_id}", file=sys.stderr)
            sys.exit(1)
        if font_id in font_ids_seen:
            print(f"Error: duplicate font_id={font_id} for font '{name}'", file=sys.stderr)
            sys.exit(1)
        font_ids_seen.add(font_id)

        header_path = project_dir / font["header"]
        data_path = project_dir / font["data"]

        header_data = bytearray(header_path.read_bytes())
        bitmap_data = data_path.read_bytes()

        # Inject font_id at byte 5 of header (first[2] + last[2] + y_advance[1] + font_id[1])
        # If header is only 5 bytes (no padding byte), insert it; otherwise overwrite byte 5
        if len(header_data) < 5:
            print(f"Error: font '{name}' header too small ({len(header_data)} bytes)", file=sys.stderr)
            sys.exit(1)
        if len(header_data) == 5:
            header_data.insert(5, font_id)
        else:
            header_data[5] = font_id

        # Combined resource: header (with font_id) + bitmap data
        combined = bytes(header_data) + bitmap_data
        resources.append((name, RES_FONT, combined, 0))

    # 2. Images — PNG → FI format (header extended to 9 bytes with image_id)
    image_ids_seen = set()
    for image in project.get("images", []):
        name = image["name"]
        image_id = image.get("image_id")
        if image_id is None:
            print(f"Error: image '{name}' missing required 'image_id' field", file=sys.stderr)
            sys.exit(1)
        if image_id == 0:
            print(f"Error: image '{name}' has image_id=0 (reserved for 'no image')", file=sys.stderr)
            sys.exit(1)
        if image_id < 0 or image_id > 254:
            print(f"Error: image '{name}' image_id must be 1-254, got {image_id}", file=sys.stderr)
            sys.exit(1)
        if image_id in image_ids_seen:
            print(f"Error: duplicate image_id={image_id} for image '{name}'", file=sys.stderr)
            sys.exit(1)
        image_ids_seen.add(image_id)

        source = project_dir / image["source"]
        mode = image.get("mode", "auto")
        max_colors = image.get("max_colors", 256)

        fi_data = bytearray(convert_image(str(source), mode=mode, max_colors=max_colors))
        # FI header is 8 bytes; extend to 9 by inserting image_id at byte 8
        # Original: magic[2] + width[2] + height[2] + flags[1] + colors[1] = 8 bytes
        # New:      ... + image_id[1] = 9 bytes
        if len(fi_data) >= 8:
            fi_data.insert(8, image_id)
        resources.append((name, RES_IMAGE, bytes(fi_data), 0))

    # 3. Pages — JSON widget tree OR .fl source → bytecode
    # Widget ID tahsisi: root(0) → page widgets → page contents
    next_id = 1  # root=0 main.rs'de, page'ler 1'den başlar

    for page in project.get("pages", []):
        name = page["name"]
        page_widget_id = next_id
        next_id += 1  # page widget'ı PageManager tarafından alloc edilir

        if "source" in page:
            # Compile .fl source as page (bg_color prefix + bytecode)
            from ferrite_lang import compile_page as fl_compile_page, CompileError
            bg_color = parse_color(page.get("background", "0x0000"))
            source_path = project_dir / page["source"]
            source = source_path.read_text(encoding="utf-8")
            try:
                page_data = fl_compile_page(source, bg_color, str(source_path), include_dirs=include_dirs)
            except CompileError as e:
                raise CompileError(f"{source_path}: {e}") from e
            # Count alloc() calls in bytecode to track widget IDs
            widget_count = source.count("alloc(")
            next_id += widget_count
        else:
            base_id = next_id  # content widget'lar buradan başlar
            page_data, widget_count = compile_page(page, page_widget_id, base_id)
            next_id += widget_count

        resources.append((name, RES_PAGE, page_data, 0))

    # 4. Programs — .fl → VM image (header + opcodes, metadata embedded)
    render_mode = project.get("render_mode", "dirty")
    for prog in project.get("programs", []):
        name = prog["name"]
        source = project_dir / prog["source"]
        image_data = compile_program(str(source), include_dirs=include_dirs,
                                     render_mode=render_mode)
        exec_mode = prog.get("exec_mode", "ram")
        prog_flags = RES_FLAG_FLASH_EXEC if exec_mode == "flash" else 0
        resources.append((name, RES_PROGRAM, image_data, prog_flags))

    # 5. Files — raw user data accessible from ferrite programs via fileOpen/Read.
    #    Max 15-char names; no processing — bytes stored verbatim.
    for f in project.get("files", []):
        name = f["name"]
        if len(name.encode("ascii")) > 15:
            print(f"Error: file name '{name}' exceeds 15 chars", file=sys.stderr)
            sys.exit(1)
        source = project_dir / f["source"]
        data = source.read_bytes()
        resources.append((name, RES_FILE, data, 0))

    # 6. Build flash binary
    fs_binary = build_fs(project, resources)
    Path(output_path).write_bytes(fs_binary)

    # Stats
    header_and_table = HEADER_SIZE + len(resources) * ENTRY_SIZE
    data_size = len(fs_binary) - header_and_table
    return {
        "resources": len(resources),
        "data_size": data_size,
        "total_size": len(fs_binary),
        "next_widget_id": next_id,
        "details": [(name, kind, len(data)) for name, kind, data, _flags in resources],
    }


def show_info(project_path: str):
    """Proje dosyasının özetini göster (build yapmadan)."""
    project = json.loads(Path(project_path).read_text(encoding="utf-8"))
    screen = project.get("screen", [800, 480])
    fonts = project.get("fonts", [])
    images = project.get("images", [])
    pages = project.get("pages", [])
    programs = project.get("programs", [])
    files = project.get("files", [])

    total_res = len(fonts) + len(images) + len(pages) + len(programs) + len(files)

    print(f"Project: {project_path}")
    print(f"Screen:  {screen[0]}×{screen[1]}")
    print(f"Fonts:   {len(fonts)}")
    print(f"Images:  {len(images)}")
    print(f"Pages:   {len(pages)}")
    print(f"Programs:{len(programs)}")
    print(f"Files:   {len(files)}")
    print(f"Total:   {total_res} resources")

    if total_res > MAX_RESOURCES:
        print(f"WARNING: {total_res} > {MAX_RESOURCES} max resources!")

    # Widget count estimate
    def count_widgets(widgets):
        c = 0
        for w in widgets:
            c += 1 + count_widgets(w.get("children", []))
        return c

    total_widgets = 1  # root
    for page in pages:
        total_widgets += 1  # page widget
        total_widgets += count_widgets(page.get("widgets", []))
    print(f"Widgets: ~{total_widgets} (max 64)")


# --- CLI ---

def main():
    args = sys.argv[1:]

    if not args or args[0] in ("-h", "--help"):
        print(__doc__)
        return

    project_path = args[0]

    if "--info" in args:
        show_info(project_path)
        return

    # Default output
    output_path = "flash.bin"
    for i, a in enumerate(args):
        if a == "-o" and i + 1 < len(args):
            output_path = args[i + 1]

    try:
        stats = build(project_path, output_path)
    except FileNotFoundError as e:
        print(f"error: file not found: {e}", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        # CompileError or other build errors — show clean message
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)

    print(f"Built: {output_path}")
    print(f"  Resources: {stats['resources']}")
    print(f"  Data size: {stats['data_size']} bytes")
    print(f"  Total:     {stats['total_size']} bytes ({stats['total_size'] / 1024:.1f} KB)")
    print(f"  Widgets:   {stats['next_widget_id']} IDs allocated")
    print()

    KIND_NAMES = {RES_FONT: "FONT", RES_IMAGE: "IMAGE", RES_PROGRAM: "PROGRAM", RES_PAGE: "PAGE", RES_FILE: "FILE"}
    for name, kind, size in stats["details"]:
        print(f"  {KIND_NAMES.get(kind, '?'):>7s}  {name:<16s}  {size:>6d} bytes")


if __name__ == "__main__":
    main()
