#!/usr/bin/env python3
"""Ferrite UI Designer — Visual layout tool for ferrite-ui widgets.

Usage:
    python ferrite_designer.py                    # new project
    python ferrite_designer.py project.fui        # open project

Requires: pip install PySide6

Manages project resources (fonts, images, programs) and generates
project.json + flash filesystem images via ferrite_build.py.
"""

import sys
import json
import os
import subprocess
import base64
import tempfile

try:
    from PySide6.QtWidgets import (
        QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
        QGraphicsScene, QGraphicsView, QGraphicsItem, QGraphicsRectItem,
        QTreeWidget, QTreeWidgetItem, QScrollArea, QFormLayout,
        QSpinBox, QLineEdit, QComboBox, QCheckBox, QPushButton,
        QLabel, QSplitter, QToolBar, QStatusBar, QGroupBox,
        QFileDialog, QColorDialog, QMessageBox, QDialog, QTextEdit,
        QDialogButtonBox, QTabWidget, QPlainTextEdit, QStyle, QDockWidget,
        QProgressDialog,
    )
    from PySide6.QtCore import Qt, QRectF, QPointF, Signal, QSize, QSettings, QRegularExpression, QBuffer
    from PySide6.QtGui import (
        QColor, QPainter, QPen, QBrush, QFont, QAction, QIcon,
        QPainterPath, QKeySequence, QSyntaxHighlighter, QTextCharFormat,
        QFontDatabase, QImage, QTextCursor,
    )
    from PySide6.QtWidgets import QListWidget, QListWidgetItem
except ImportError:
    print("PySide6 is required: pip install PySide6")
    sys.exit(1)

try:
    import qtawesome as qta
except ImportError:
    qta = None



# Designer UI icons live in tools/images/ (in-repo, relative to this script)
_DESIGNER_ICONS_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "images")

# SVG icon browser folder (user-configurable, for importing icons into ferrite projects)
SVG_ICON_FOLDER_KEY = "svg_icon_folder"
SVG_ICON_FOLDER_DEFAULT = r"E:\ico\images"

# Maps icon names (qtawesome or logical) to filename stems in _DESIGNER_ICONS_DIR
_ICON_MAP = {
    # File / project
    "fa5s.file":                "NewDocument",
    "fa5s.folder-open":         "OpenFolder",
    "fa5s.save":                "Save",
    "fa5s.file-export":         "Export",
    # Build / deploy
    "fa5s.hammer":              "BuildDefinition",
    "fa5s.upload":              "Upload",
    "fa5s.cloud-upload-alt":    "CloudUpload",
    "fa5s.sync":                "Refresh",
    "fa5s.play":                "Run",
    # Edit
    "fa5s.trash":               "Delete",
    "fa5s.copy":                "Copy",
    "fa5s.paste":               "Paste",
    "fa5s.clone":               "Duplicate",
    "fa5s.arrow-up":            "BringToFront",
    "fa5s.arrow-down":          "SendBackward",
    "fa5s.th":                  "SnapToGrid",
    # Align (used in align-action tuples)
    "fa5s.align-left":          "AlignLeft",
    "fa5s.align-right":         "AlignRight",
    "fa5s.align-top":           "AlignTop",
    "fa5s.align-bottom":        "AlignBottom",
    "fa5s.align-center":        "AlignCenter",
    "fa5s.center-h":            "CenterHorizontally",
    "fa5s.center-v":            "CenterVertically",
    # Code / resources
    "fa5s.code":                "Code",
    "fa5s.file-code":           "Code",
    "fa5s.font":                "Font",
    "fa5s.image":               "Image",
    "fa5s.icons":               "ImageButton",
    "fa5s.undo":                "Refresh",
    # Comms / device
    "fa5s.satellite-dish":      "Connect",
    "fa5s.power-off":           "Restart",
    "fa5s.memory":              "Memory",
    "fa5s.microchip":           "Processor",
    "fa5s.expand-arrows-alt":   "FitToScreen",
    # Close / exit
    "fa5s.times":               "Close",
    "exit":                     "Exit",
    # Widget kinds
    "fa5s.square":              "Panel",
    "fa5s.hand-pointer":        "Button",
    "fa5s.tasks":               "ProgressBar",
    "fa5s.sliders-h":           "Slider",
    "fa5s.check-square":        "CheckBoxChecked",
    "fa5s.dot-circle":          "RadioButton",
    "fa5s.caret-square-down":   "ComboBox",
}

def designer_icon(name, fallback=None):
    """Return a QIcon: SVG collection first, then qtawesome, then Qt built-in."""
    # 1. SVG collection via _ICON_MAP (tools/images/ inside repo)
    stem = _ICON_MAP.get(name)
    if stem:
        path = os.path.join(_DESIGNER_ICONS_DIR, stem + ".svg")
        if os.path.isfile(path):
            icon = QIcon(path)
            if not icon.isNull():
                return icon
    # 2. qtawesome (legacy / unmapped names)
    if qta is not None and name:
        try:
            return qta.icon(name)
        except Exception:
            pass
    # 3. Qt standard icon
    if fallback is not None:
        return QApplication.style().standardIcon(fallback)
    return None


def set_icon(widget, name, fallback=None):
    icon = designer_icon(name, fallback)
    if icon is not None:
        widget.setIcon(icon)


# ============================================================
# Constants
# ============================================================

# Device definitions come from tools/devices/*.json via the shared loader.
from ferrite_devices import (
    DEFAULT_DEVICE_ID,
    device_spec,
    infer_device_id,
    load_devices,
)

KIND_BASE = 0
KIND_LABEL = 1
KIND_BUTTON = 2
KIND_PROGRESS = 3
KIND_SLIDER = 4
KIND_CHECKBOX = 5
KIND_RADIO = 6
KIND_DROPDOWN = 9
KIND_IMAGE = 11

KIND_NAMES = ["Base", "Label", "Button", "Progress", "Slider", "Checkbox", "Radio", "Dropdown", "", "Image"]
KIND_VALUES = [KIND_BASE, KIND_LABEL, KIND_BUTTON, KIND_PROGRESS, KIND_SLIDER, KIND_CHECKBOX, KIND_RADIO, KIND_DROPDOWN, KIND_IMAGE]
KIND_PREFIXES = {
    KIND_BASE: "panel",
    KIND_LABEL: "lbl",
    KIND_BUTTON: "btn",
    KIND_PROGRESS: "progress",
    KIND_SLIDER: "slider",
    KIND_CHECKBOX: "cb",
    KIND_RADIO: "radio",
    KIND_DROPDOWN: "dropdown",
    KIND_IMAGE: "img",
}

KIND_ICON_NAMES = {
    KIND_BASE:     "fa5s.square",
    KIND_LABEL:    "fa5s.font",
    KIND_BUTTON:   "fa5s.hand-pointer",
    KIND_PROGRESS: "fa5s.tasks",
    KIND_SLIDER:   "fa5s.sliders-h",
    KIND_CHECKBOX: "fa5s.check-square",
    KIND_RADIO:    "fa5s.dot-circle",
    KIND_DROPDOWN: "fa5s.caret-square-down",
    KIND_IMAGE:    "fa5s.image",
}


def kind_name(kind):
    if kind in KIND_VALUES:
        return KIND_NAMES[KIND_VALUES.index(kind)]
    return "?"

HANDLE_SIZE = 8
CANVAS_MARGIN = 40
GRID_SIZE = 10
GUIDE_SNAP_DIST = 6


# ============================================================
# Color utilities
# ============================================================

def rgb565_to_qcolor(c):
    r = ((c >> 11) & 0x1F) * 255 // 31
    g = ((c >> 5) & 0x3F) * 255 // 63
    b = (c & 0x1F) * 255 // 31
    return QColor(r, g, b)


def qcolor_to_rgb565(qc):
    return ((qc.red() >> 3) << 11) | ((qc.green() >> 2) << 5) | (qc.blue() >> 3)


def rgb565_to_gray4(c):
    r = ((c >> 11) & 0x1F) * 255 // 31
    g = ((c >> 5) & 0x3F) * 255 // 63
    b = (c & 0x1F) * 255 // 31
    gray = (r * 30 + g * 59 + b * 11) // 100
    return min(15, max(0, round(gray * 15 / 255)))


def gray4_to_rgb565(gray4):
    v = min(15, max(0, int(gray4))) * 255 // 15
    return ((v >> 3) << 11) | ((v >> 2) << 5) | (v >> 3)


def quantize_color_for_device(color, device_id):
    color &= 0xFFFF
    if device_spec(device_id)["color_model"] == "4-bit grayscale":
        return gray4_to_rgb565(rgb565_to_gray4(color))
    return color


# ============================================================
# Resource preview caches
# ============================================================

import struct as _struct
from PySide6.QtGui import QImage, QPixmap


def _is_sparse_font(data):
    """Return True if data looks like a new sparse font binary (not old range format).

    Old format: bytes 0-1 = first, bytes 2-3 = last (both small codepoints like 0x0020/0x007E).
    New format: bytes 0-1 = num_glyphs (e.g. 95 for ASCII), byte 2 = y_advance, byte 3 = font_id.
    Heuristic: if byte 3 (font_id candidate) <= 254 AND the codepoints section is consistent
    with a sparse layout, treat as new format.  The simplest check: new format has
    num_glyphs u16 at [0], then at [4] the first codepoint — which should equal the first
    char in the set.  For old format bytes [2..4] = last u16, which would be e.g. 0x007E.
    Distinguish: in old format data[4] is y_advance (typically 10-30), in new format data[4]
    is the low byte of the first codepoint (0x20 for space = 32).  Use a round-trip check:
    parse as new, verify the first codepoint matches.
    """
    if len(data) < 4:
        return False
    # V2 (2bpp anti-aliased): magic bytes 0x00, 0x02 at positions 0-1.
    if len(data) >= 7 and data[0] == 0x00 and data[1] == 0x02:
        return True
    n = _struct.unpack_from("<H", data, 0)[0]
    if n == 0 or n > 0x3FFF:
        return False
    cp_end = 4 + n * 2
    glyph_end = cp_end + n * 7
    if len(data) < glyph_end:
        return False
    # Verify codepoints are sorted ascending
    prev = -1
    for i in range(min(n, 4)):
        cp = _struct.unpack_from("<H", data, 4 + i * 2)[0]
        if cp <= prev:
            return False
        prev = cp
    return True


def _old_range_font_to_sparse(data):
    """Convert old range-based font binary to new sparse format.

    Old layout: first(u16) + last(u16) + y_advance(u8) + font_id(u8) + glyphs(N*7) + bitmap
    New layout: num_glyphs(u16) + y_advance(u8) + font_id(u8) + codepoints(N*u16) + glyphs(N*7) + bitmap
    """
    if len(data) < 6:
        return None
    first, last = _struct.unpack_from("<HH", data, 0)
    y_advance = data[4]
    font_id = data[5]
    n = last - first + 1
    header_size = 6 + n * 7
    if len(data) < header_size or n <= 0 or n > 0x3FFF:
        return None
    glyphs_raw = data[6:header_size]
    bitmap = data[header_size:]
    out = bytearray()
    out.extend(_struct.pack("<HBB", n, y_advance, font_id))
    for cp in range(first, last + 1):
        out.extend(_struct.pack("<H", cp))
    out.extend(glyphs_raw)
    out.extend(bitmap)
    return bytes(out)


class GfxFontPreview:
    """Parses Ferrite sparse binary font data for QPainter rendering."""

    def __init__(self, binary):
        if len(binary) < 4:
            raise ValueError("font binary too short")
        # V2 (2bpp): 7-byte header  magic(0x00,0x02) + n u16 + y_adv + font_id + bpp
        # V1 (1bpp): 4-byte header  n u16 + y_adv + font_id
        if len(binary) >= 7 and binary[0] == 0x00 and binary[1] == 0x02:
            n = _struct.unpack_from("<H", binary, 2)[0]
            self.y_advance = binary[4]
            self.bpp = binary[6]
            hdr_len = 7
        else:
            n = _struct.unpack_from("<H", binary, 0)[0]
            self.y_advance = binary[2]
            self.bpp = 1
            hdr_len = 4
        # font_id not needed for rendering
        cp_end = hdr_len + n * 2
        glyph_end = cp_end + n * 7
        if len(binary) < glyph_end:
            raise ValueError("font binary truncated")
        self.codepoints = [_struct.unpack_from("<H", binary, hdr_len + i * 2)[0]
                           for i in range(n)]
        self.glyphs = []  # (bitmap_offset, w, h, x_advance, x_offset, y_offset)
        for i in range(n):
            off = cp_end + i * 7
            bm_off = _struct.unpack_from("<H", binary, off)[0]
            w = binary[off + 2]
            h = binary[off + 3]
            x_adv = binary[off + 4]
            x_off = binary[off + 5] if binary[off + 5] < 128 else binary[off + 5] - 256
            y_off = binary[off + 6] if binary[off + 6] < 128 else binary[off + 6] - 256
            self.glyphs.append((bm_off, w, h, x_adv, x_off, y_off))
        self.bitmap = binary[glyph_end:]

    def _find(self, cp):
        """Binary search for codepoint cp; returns glyph index or -1."""
        lo, hi = 0, len(self.codepoints) - 1
        while lo <= hi:
            mid = (lo + hi) >> 1
            c = self.codepoints[mid]
            if c == cp:
                return mid
            elif c < cp:
                lo = mid + 1
            else:
                hi = mid - 1
        return -1

    def text_width(self, text):
        w = 0
        for ch in text:
            idx = self._find(ord(ch))
            if idx >= 0:
                w += self.glyphs[idx][3]  # x_advance
        return w

    def line_height(self):
        return self.y_advance

    def text_bounds(self, text):
        """Return (ascent, descent) in pixels for the given string.

        ascent  — pixels above baseline (positive)
        descent — pixels below baseline (positive)
        """
        ascent = 0
        descent = 0
        for ch in text:
            idx = self._find(ord(ch))
            if idx < 0:
                continue
            _, gw, gh, _, _, y_off = self.glyphs[idx]
            if gw == 0 or gh == 0:
                continue
            ascent = max(ascent, -y_off)           # y_off is negative above baseline
            descent = max(descent, y_off + gh)     # positive if below baseline
        return ascent, descent

    def draw_text(self, painter, x, y, text, color):
        """Draw text at (x, y=baseline).

        1bpp: opaque foreground pixels.  2bpp: anti-aliased — each pixel's
        2-bit gray level (0-3) maps to an alpha so the glyph blends with
        whatever is painted underneath (mirrors the hardware fg/bg blend).
        """
        pen = painter.pen()
        painter.setPen(Qt.NoPen)
        if self.bpp == 1:
            brush = QBrush(color)
            for ch in text:
                idx = self._find(ord(ch))
                if idx < 0:
                    continue
                bm_off, gw, gh, x_adv, x_off, y_off = self.glyphs[idx]
                if gw == 0 or gh == 0:
                    x += x_adv
                    continue
                gx = x + x_off
                gy = y + y_off
                bit = 0
                for row in range(gh):
                    for col in range(gw):
                        byte_idx = bm_off + (bit >> 3)
                        if byte_idx < len(self.bitmap):
                            if self.bitmap[byte_idx] & (0x80 >> (bit & 7)):
                                painter.fillRect(gx + col, gy + row, 1, 1, brush)
                        bit += 1
                x += x_adv
        else:
            # 2bpp: 4 pixels per byte, MSB first (shifts 6,4,2,0).  Pre-build a
            # brush per non-zero gray level (alpha = level/3).
            r, g, b = color.red(), color.green(), color.blue()
            brushes = {lvl: QBrush(QColor(r, g, b, (lvl * 255) // 3))
                       for lvl in (1, 2, 3)}
            for ch in text:
                idx = self._find(ord(ch))
                if idx < 0:
                    continue
                bm_off, gw, gh, x_adv, x_off, y_off = self.glyphs[idx]
                if gw == 0 or gh == 0:
                    x += x_adv
                    continue
                gx = x + x_off
                gy = y + y_off
                p = 0
                for row in range(gh):
                    for col in range(gw):
                        byte_idx = bm_off + (p >> 2)
                        if byte_idx < len(self.bitmap):
                            shift = 6 - 2 * (p & 3)
                            level = (self.bitmap[byte_idx] >> shift) & 0x03
                            if level:
                                painter.fillRect(gx + col, gy + row, 1, 1,
                                                 brushes[level])
                        p += 1
                x += x_adv
        painter.setPen(pen)


class ResourceCache:
    """Lazy cache for font previews and image pixmaps."""

    def __init__(self, model):
        self.model = model
        self._fonts = {}   # font_id -> GfxFontPreview or None
        self._images = {}  # image_id -> QPixmap or None
        self._gen = -1

    def invalidate(self):
        self._fonts.clear()
        self._images.clear()
        self._gen = -1

    def _check_gen(self):
        gen = self.model._res_gen
        if gen != self._gen:
            self._fonts.clear()
            self._images.clear()
            self._gen = gen

    def get_font(self, font_id):
        """Return GfxFontPreview for font_id, or None if not available."""
        if font_id == 0:
            return None
        self._check_gen()
        if font_id in self._fonts:
            return self._fonts[font_id]
        for fres in self.model.fonts:
            if fres.get("font_id") != font_id:
                continue
            try:
                data = base64.b64decode(fres.get("combined_b64", ""))
                if len(data) >= 4:
                    font = GfxFontPreview(data)
                    self._fonts[font_id] = font
                    return font
            except Exception:
                pass
            break
        self._fonts[font_id] = None
        return None

    def get_image(self, image_id):
        """Return QPixmap for image_id, or None if not available."""
        if image_id == 0:
            return None
        self._check_gen()
        if image_id in self._images:
            return self._images[image_id]
        for ires in self.model.images:
            if ires.get("image_id") != image_id:
                continue
            try:
                raw = base64.b64decode(ires["data_b64"])
                img = QImage()
                if img.loadFromData(raw):
                    self._images[image_id] = QPixmap.fromImage(img)
                    return self._images[image_id]
            except Exception:
                pass
            break
        self._images[image_id] = None
        return None


# ============================================================
# Data model
# ============================================================

class WidgetNode:
    """Single widget with all ferrite properties."""

    def __init__(self, name, kind=KIND_BASE):
        self.name = name
        self.kind = kind
        self.parent = None
        self.children = []

        self.loc_x = 0
        self.loc_y = 0
        self.size_w = 100
        self.size_h = 40

        self.margin = [0, 0, 0, 0]
        self.border = [0, 0, 0, 0]
        self.padding = [0, 0, 0, 0]

        self.bg_color = 0x0000
        self.border_color = 0x0000
        self.border_radius = 0
        self.text_color = 0xFFFF
        self.press_color = 0x0000
        self.alpha = 255  # background opacity (alpha-capable devices only)

        self.text = ""
        self.font_id = 0
        self.text_align = 0

        self.visible = True
        self.enabled = True
        self.clickable = False
        self.value = 0
        self.checked = False
        self.multi_line = False
        self.image_id = 0

        self.on_click = ""
        self.on_tap = ""
        self.on_paint = ""

    def abs_pos(self):
        x, y = self.loc_x, self.loc_y
        p = self.parent
        while p is not None:
            x += p.loc_x + p.border[3] + p.padding[3]
            y += p.loc_y + p.border[0] + p.padding[0]
            p = p.parent
        return x, y

    def content_origin(self):
        ax, ay = self.abs_pos()
        return ax + self.border[3] + self.padding[3], ay + self.border[0] + self.padding[0]

    def descendants(self):
        result = set()
        stack = list(self.children)
        while stack:
            c = stack.pop()
            result.add(c)
            stack.extend(c.children)
        return result

    def to_dict(self):
        return {
            "name": self.name, "kind": self.kind,
            "parent": self.parent.name if self.parent else None,
            "loc_x": self.loc_x, "loc_y": self.loc_y,
            "size_w": self.size_w, "size_h": self.size_h,
            "margin": self.margin[:], "border": self.border[:],
            "padding": self.padding[:],
            "bg_color": self.bg_color, "border_color": self.border_color,
            "border_radius": self.border_radius,
            "text_color": self.text_color, "press_color": self.press_color,
            "alpha": self.alpha,
            "text": self.text, "font_id": self.font_id, "text_align": self.text_align,
            "visible": self.visible, "enabled": self.enabled,
            "clickable": self.clickable,
            "value": self.value, "checked": self.checked, "multi_line": self.multi_line,
            "image_id": self.image_id,
            "on_click": self.on_click, "on_tap": self.on_tap, "on_paint": self.on_paint,
        }

    @staticmethod
    def from_dict(d):
        n = WidgetNode(d["name"], d.get("kind", 0))
        for key in ("loc_x", "loc_y", "size_w", "size_h", "bg_color", "border_color",
                     "border_radius", "text_color", "press_color", "alpha", "text",
                     "font_id", "text_align", "visible", "enabled", "clickable", "value",
                     "checked", "multi_line", "image_id", "on_click", "on_tap", "on_paint"):
            if key in d:
                setattr(n, key, d[key])
        for key in ("margin", "border", "padding"):
            if key in d:
                setattr(n, key, list(d[key]))
        return n


class DesignerModel(QWidget):
    """Central model owning widget tree, selection, and signals."""
    changed = Signal()
    tree_changed = Signal()
    selection_changed = Signal()

    def __init__(self, device_id=DEFAULT_DEVICE_ID):
        super().__init__()
        self.device_id = device_id
        spec = device_spec(self.device_id)
        self.screen_w, self.screen_h = spec["screen"]
        self.root = WidgetNode("root", KIND_BASE)
        self.root.size_w = self.screen_w
        self.root.size_h = self.screen_h
        self.root.bg_color = spec["root_bg"]
        self.widgets = [self.root]
        self.selected = None
        self._counter = 0
        self._path = None
        self.snap_to_grid = False
        self.selection = set()   # all currently selected WidgetNodes
        self.clipboard = None    # dict from WidgetNode.to_dict()

        # Project resources — all binary data embedded as base64 in .fui
        self.fonts = []     # [{"name": str, "font_id": int, "header_b64": str, "data_b64": str}]
        self.images = []    # [{"name": str, "image_id": int, "source_name": str, "mode": "auto", "max_colors": 256, "data_b64": str}]
        self.programs = []  # [{"name": str, "exec_mode": "ram"|"flash", "source_b64": str}]
        self.files = []     # [{"name": str, "source_name": str, "data_b64": str}]
        self._res_gen = 0   # bumped when fonts/images change, for cache invalidation
        self.include_dirs = []  # extra include dirs for compilation
        self.exec_mode = "flash"  # default exec_mode for the main program
        self.render_mode = spec["render_mode"]  # "dirty", "buffered", or "epaper"
        self.main_fl = DEFAULT_MAIN_FL  # user code — embedded in .fui

    def set_device(self, device_id):
        self.device_id = device_id if device_id in load_devices() else DEFAULT_DEVICE_ID
        spec = device_spec(self.device_id)
        self.screen_w, self.screen_h = spec["screen"]
        self.render_mode = spec["render_mode"]

    def add_widget(self, kind, parent=None):
        if parent is None:
            parent = self.selected if self.selected else self.root
        self._counter += 1
        name = f"{KIND_PREFIXES.get(kind, 'widget')}{self._counter}"
        while any(w.name == name for w in self.widgets):
            self._counter += 1
            name = f"{KIND_PREFIXES.get(kind, 'widget')}{self._counter}"
        node = WidgetNode(name, kind)
        node.parent = parent
        parent.children.append(node)
        if kind == KIND_LABEL:
            node.text = "Label"
            node.bg_color = 0x0000
            node.text_color = 0xFFFF
        elif kind == KIND_BUTTON:
            node.size_w = 120
            node.size_h = 50
            node.bg_color = 0x001F
            node.press_color = 0x0010
            node.clickable = True
        elif kind == KIND_PROGRESS:
            node.size_w = 200
            node.size_h = 20
            node.bg_color = 0x2104
            node.press_color = 0x07E0
            node.value = 50
        elif kind == KIND_SLIDER:
            node.size_w = 200
            node.size_h = 30
            node.bg_color = 0x2104
            node.press_color = 0x001F
            node.border_color = 0xFFFF
            node.clickable = True
            node.value = 50
        elif kind == KIND_CHECKBOX:
            node.size_w = 30
            node.size_h = 30
            node.bg_color = 0x2104
            node.text_color = 0xFFFF
            node.press_color = 0x07E0
            node.clickable = True
        elif kind == KIND_RADIO:
            node.size_w = 30
            node.size_h = 30
            node.bg_color = 0x2104
            node.text_color = 0xFFFF
            node.press_color = 0x001F
            node.clickable = True
        elif kind == KIND_DROPDOWN:
            node.size_w = 220
            node.size_h = 38
            node.text = "Dropdown"
            node.bg_color = 0x2104
            node.border_color = 0xFFFF
            node.text_color = 0xFFFF
            node.clickable = True
        if self.device_id == "epaper":
            node.text_color = 0x0000
            if kind == KIND_LABEL:
                node.bg_color = 0x0000  # transparent in generated Ferrite code
            elif kind in (KIND_BUTTON, KIND_DROPDOWN):
                node.bg_color = 0xBDF7
                node.press_color = 0x7BEF
            elif kind in (KIND_PROGRESS, KIND_SLIDER):
                node.bg_color = 0xDEFB
                node.press_color = 0x4208
            elif kind in (KIND_CHECKBOX, KIND_RADIO):
                node.bg_color = 0xFFFF
                node.press_color = 0x4208
        node.bg_color = quantize_color_for_device(node.bg_color, self.device_id)
        node.border_color = quantize_color_for_device(node.border_color, self.device_id)
        node.text_color = quantize_color_for_device(node.text_color, self.device_id)
        node.press_color = quantize_color_for_device(node.press_color, self.device_id)
        self.widgets.append(node)
        self.tree_changed.emit()
        self.select(node)
        return node

    def remove_widget(self, node):
        if node is self.root:
            return
        desc = node.descendants()
        desc.add(node)
        node.parent.children.remove(node)
        self.widgets = [w for w in self.widgets if w not in desc]
        if self.selected in desc:
            self.select(None)
        self.tree_changed.emit()

    def reparent(self, node, new_parent):
        if node is self.root or new_parent is node:
            return
        if new_parent in node.descendants():
            return
        if node.parent:
            node.parent.children.remove(node)
        node.parent = new_parent
        new_parent.children.append(node)
        self.tree_changed.emit()

    def bring_to_front(self, node):
        if node is self.root or node.parent is None:
            return
        siblings = node.parent.children
        if siblings[-1] is node:
            return
        siblings.remove(node)
        siblings.append(node)
        self.tree_changed.emit()

    def send_to_back(self, node):
        if node is self.root or node.parent is None:
            return
        siblings = node.parent.children
        if siblings[0] is node:
            return
        siblings.remove(node)
        siblings.insert(0, node)
        self.tree_changed.emit()

    def select(self, node):
        self.selected = node
        self.selection = {node} if node else set()
        self.selection_changed.emit()

    def select_toggle(self, node):
        """Ctrl+click: add/remove node from multi-selection."""
        if node in self.selection:
            self.selection.discard(node)
            if node is self.selected:
                self.selected = next(iter(self.selection), None)
        else:
            self.selection.add(node)
            self.selected = node
        self.selection_changed.emit()

    def copy_selected(self):
        if self.selected and self.selected is not self.root:
            self.clipboard = self.selected.to_dict()

    def paste_widget(self):
        if not self.clipboard:
            return None
        parent = (self.selected.parent if self.selected and self.selected is not self.root
                  else self.root)
        if parent is None:
            parent = self.root
        d = dict(self.clipboard)
        d["loc_x"] = d.get("loc_x", 0) + 10
        d["loc_y"] = d.get("loc_y", 0) + 10
        base = self.clipboard.get("name", "widget")
        name = base + "_copy"
        n = 1
        while any(w.name == name for w in self.widgets):
            name = f"{base}_copy{n}"
            n += 1
        d["name"] = name
        node = WidgetNode.from_dict(d)
        node.parent = parent
        parent.children.append(node)
        self._counter += 1
        self.widgets.append(node)
        self.tree_changed.emit()
        self.select(node)
        return node

    def notify_changed(self):
        self.changed.emit()

    def snap(self, val):
        if self.snap_to_grid:
            return round(val / GRID_SIZE) * GRID_SIZE
        return val

    def find_by_name(self, name):
        for w in self.widgets:
            if w.name == name:
                return w
        return None

    def dfs_order(self):
        result = []
        stack = [self.root]
        while stack:
            node = stack.pop()
            result.append(node)
            for c in reversed(node.children):
                stack.append(c)
        return result

    def clear(self, device_id=None):
        if device_id is not None:
            self.set_device(device_id)
        spec = device_spec(self.device_id)
        self.root = WidgetNode("root", KIND_BASE)
        self.root.size_w = self.screen_w
        self.root.size_h = self.screen_h
        self.root.bg_color = spec["root_bg"]
        self.widgets = [self.root]
        self.selected = None
        self._counter = 0
        self.fonts = []
        self.images = []
        self.programs = []
        self.files = []
        self.include_dirs = []
        self.exec_mode = "flash"
        self.render_mode = spec["render_mode"]
        self.main_fl = DEFAULT_MAIN_FL
        self.tree_changed.emit()
        self.selection_changed.emit()

    def save_to_file(self, path):
        data = {
            "version": 2,
            "device": self.device_id,
            "screen": [self.screen_w, self.screen_h],
            "color_model": device_spec(self.device_id)["color_model"],
            "counter": self._counter,
            "widgets": [w.to_dict() for w in self.dfs_order()],
            "main_fl": base64.b64encode(self.main_fl.encode("utf-8")).decode("ascii"),
            "fonts": self.fonts[:],
            "images": self.images[:],
            "programs": self.programs[:],
            "files": self.files[:],
            "include_dirs": self.include_dirs[:],
            "exec_mode": self.exec_mode,
            "render_mode": self.render_mode,
        }
        with open(path, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2)
        self._path = path

    def load_from_file(self, path):
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        screen = data.get("screen")
        has_render_mode = "render_mode" in data
        render_mode = data.get("render_mode", "dirty")
        self.device_id = data.get("device") or infer_device_id(screen, render_mode)
        if self.device_id not in load_devices():
            self.device_id = DEFAULT_DEVICE_ID
        spec = device_spec(self.device_id)
        self.screen_w, self.screen_h = tuple(screen) if screen else spec["screen"]
        if not has_render_mode:
            render_mode = spec["render_mode"]
        nodes = {}
        order = []
        for wd in data.get("widgets", []):
            node = WidgetNode.from_dict(wd)
            nodes[node.name] = node
            order.append((node, wd.get("parent")))
        for node, pname in order:
            if pname and pname in nodes:
                node.parent = nodes[pname]
                nodes[pname].children.append(node)
        self.root = nodes.get("root", order[0][0] if order else WidgetNode("root"))
        self.root.size_w = self.screen_w
        self.root.size_h = self.screen_h
        self.widgets = [n for n, _ in order] if order else [self.root]
        self._counter = data.get("counter", len(self.widgets))

        # main.fl — embedded or from disk (backward compat)
        if "main_fl" in data:
            self.main_fl = base64.b64decode(data["main_fl"]).decode("utf-8")
        else:
            self.main_fl = DEFAULT_MAIN_FL

        # Resources — migrate old path-based / old range-based formats to embedded sparse
        self.fonts = []
        for f_entry in data.get("fonts", []):
            if "combined_b64" in f_entry:
                combined = base64.b64decode(f_entry["combined_b64"])
                # If old range-based format (starts with first/last u16 pair, 6-byte header)
                if len(combined) >= 6 and not _is_sparse_font(combined):
                    combined = _old_range_font_to_sparse(combined) or combined
                self.fonts.append({
                    "name": f_entry["name"], "font_id": f_entry["font_id"],
                    "combined_b64": base64.b64encode(combined).decode("ascii"),
                })
            elif "header_b64" in f_entry and "data_b64" in f_entry:
                # Old split format: header (first+last+y_adv+font_id+glyphs) + bitmap
                combined = (base64.b64decode(f_entry["header_b64"])
                            + base64.b64decode(f_entry["data_b64"]))
                if _is_sparse_font(combined):
                    converted = combined
                else:
                    converted = _old_range_font_to_sparse(combined) or combined
                self.fonts.append({
                    "name": f_entry["name"], "font_id": f_entry["font_id"],
                    "combined_b64": base64.b64encode(converted).decode("ascii"),
                })
            elif "header" in f_entry:
                # Old format: read files from disk and embed
                proj_dir = os.path.dirname(os.path.abspath(path))
                try:
                    h_path = os.path.join(proj_dir, f_entry["header"])
                    d_path = os.path.join(proj_dir, f_entry["data"])
                    combined = open(h_path, "rb").read() + open(d_path, "rb").read()
                    converted = _old_range_font_to_sparse(combined) or combined
                    self.fonts.append({
                        "name": f_entry["name"], "font_id": f_entry["font_id"],
                        "combined_b64": base64.b64encode(converted).decode("ascii"),
                    })
                except Exception:
                    pass  # skip unreadable fonts

        self.images = []
        for i_entry in data.get("images", []):
            if "data_b64" in i_entry:
                self.images.append(i_entry)
            elif "source" in i_entry:
                proj_dir = os.path.dirname(os.path.abspath(path))
                try:
                    s_path = os.path.join(proj_dir, i_entry["source"])
                    s_b64 = base64.b64encode(open(s_path, "rb").read()).decode("ascii")
                    self.images.append({
                        "name": i_entry["name"], "image_id": i_entry["image_id"],
                        "source_name": os.path.basename(i_entry["source"]),
                        "mode": i_entry.get("mode", "auto"),
                        "max_colors": i_entry.get("max_colors", 256),
                        "data_b64": s_b64,
                    })
                except Exception:
                    pass

        self.programs = []
        for p_entry in data.get("programs", []):
            if "source_b64" in p_entry:
                self.programs.append(p_entry)
            elif "source" in p_entry:
                proj_dir = os.path.dirname(os.path.abspath(path))
                try:
                    s_path = os.path.join(proj_dir, p_entry["source"])
                    s_b64 = base64.b64encode(open(s_path, "rb").read_text("utf-8").encode("utf-8")).decode("ascii")
                    self.programs.append({
                        "name": p_entry["name"],
                        "exec_mode": p_entry.get("exec_mode", "ram"),
                        "source_b64": s_b64,
                    })
                except Exception:
                    pass

        self.files = []
        for fi_entry in data.get("files", []):
            if "data_b64" in fi_entry:
                self.files.append(fi_entry)
            elif "source" in fi_entry:
                proj_dir = os.path.dirname(os.path.abspath(path))
                try:
                    s_path = os.path.join(proj_dir, fi_entry["source"])
                    s_b64 = base64.b64encode(open(s_path, "rb").read()).decode("ascii")
                    self.files.append({
                        "name": fi_entry["name"],
                        "source_name": os.path.basename(fi_entry["source"]),
                        "data_b64": s_b64,
                    })
                except Exception:
                    pass

        self.include_dirs = data.get("include_dirs", [])
        self.exec_mode = data.get("exec_mode", "flash")
        self.render_mode = render_mode
        self._path = path
        self._res_gen += 1
        self.selected = None
        self.tree_changed.emit()
        self.selection_changed.emit()

    def project_dir(self):
        if self._path:
            return os.path.dirname(os.path.abspath(self._path))
        return None

    def extract_to_dir(self, build_dir):
        """Extract all embedded resources to a build directory for compilation.

        Writes: main.fl, main.designer.fl, font bins, image files, extra .fl programs, project.json.
        Returns the project.json path.
        """
        os.makedirs(build_dir, exist_ok=True)

        # main.designer.fl — auto-generated from widget tree
        from ferrite_designer import generate_designer_fl
        designer_fl = generate_designer_fl(self)
        with open(os.path.join(build_dir, "main.designer.fl"), "w", encoding="utf-8") as f:
            f.write(designer_fl)

        # main.fl — user code
        with open(os.path.join(build_dir, "main.fl"), "w", encoding="utf-8") as f:
            f.write(self.main_fl)

        # Fonts — extract as single sparse binary for ferrite_build.py
        font_entries = []
        for font in self.fonts:
            name = font["name"]
            font_file = f"{name}.bin"
            with open(os.path.join(build_dir, font_file), "wb") as f:
                f.write(base64.b64decode(font["combined_b64"]))
            font_entries.append({
                "name": name, "font_id": font["font_id"],
                "file": font_file,
            })

        # Images — extract source files
        image_entries = []
        for img in self.images:
            name = img["name"]
            src_name = img.get("source_name", f"{name}.png")
            with open(os.path.join(build_dir, src_name), "wb") as f:
                f.write(base64.b64decode(img["data_b64"]))
            image_entries.append({
                "name": name, "image_id": img["image_id"],
                "source": src_name,
                "mode": img.get("mode", "auto"),
                "max_colors": img.get("max_colors", 256),
            })

        # Extra programs — extract .fl sources
        prog_entries = []
        for prog in self.programs:
            name = prog["name"]
            src_file = f"{name}.fl"
            with open(os.path.join(build_dir, src_file), "w", encoding="utf-8") as f:
                f.write(base64.b64decode(prog["source_b64"]).decode("utf-8"))
            prog_entries.append({
                "name": name, "source": src_file,
                "exec_mode": prog.get("exec_mode", "ram"),
            })

        # Files — extract raw user data
        file_entries = []
        for fi in self.files:
            src_name = fi.get("source_name", fi["name"])
            with open(os.path.join(build_dir, src_name), "wb") as f:
                f.write(base64.b64decode(fi["data_b64"]))
            file_entries.append({"name": fi["name"], "source": src_name})

        # project.json
        dirs = list(self.include_dirs)
        if "." not in dirs:
            dirs.insert(0, ".")
        proj = {
            "version": 2,
            "device": self.device_id,
            "screen": [self.screen_w, self.screen_h],
            "include_dirs": dirs,
            "render_mode": self.render_mode,
            "fonts": font_entries,
            "images": image_entries,
            "programs": [
                {"name": "main", "source": "main.fl", "exec_mode": self.exec_mode}
            ] + prog_entries,
            "files": file_entries,
        }
        json_path = os.path.join(build_dir, "project.json")
        with open(json_path, "w", encoding="utf-8") as f:
            json.dump(proj, f, indent=2)
        return json_path


# ============================================================
# Canvas items
# ============================================================

class WidgetItem(QGraphicsItem):
    """Visual representation of a widget on the canvas."""

    def __init__(self, node, model, res_cache=None):
        super().__init__()
        self.node = node
        self.model = model
        self.res_cache = res_cache
        self._dragging = False
        self._drag_start = None
        self._orig_loc = (0, 0)
        self._drag_orig_all = {}
        self.setFlag(QGraphicsItem.ItemIsSelectable, True)
        self.setAcceptHoverEvents(True)

    def boundingRect(self):
        return QRectF(0, 0, self.node.size_w, self.node.size_h)

    def paint(self, painter, option, widget=None):
        node = self.node
        w, h = node.size_w, node.size_h
        r = node.border_radius

        painter.setRenderHint(QPainter.Antialiasing, r > 0)

        # Background
        bg = rgb565_to_qcolor(node.bg_color)
        if node.bg_color == 0 and node is not self.model.root:
            bg = QColor(0, 0, 0, 0)
        elif node.alpha < 255 and device_spec(self.model.device_id)["caps"].get("alpha"):
            bg.setAlpha(node.alpha)

        if r > 0:
            path = QPainterPath()
            path.addRoundedRect(QRectF(0, 0, w, h), r, r)
            painter.fillPath(path, QBrush(bg))
        else:
            painter.fillRect(QRectF(0, 0, w, h), bg)

        # Border
        bw = node.border
        if any(b > 0 for b in bw) and node.border_color != 0:
            pen = QPen(rgb565_to_qcolor(node.border_color), max(bw))
            painter.setPen(pen)
            painter.setBrush(Qt.NoBrush)
            if r > 0:
                painter.drawRoundedRect(QRectF(1, 1, w - 2, h - 2), r, r)
            else:
                painter.drawRect(QRectF(0.5, 0.5, w - 1, h - 1))

        # Progress / Slider fill
        if node.kind in (KIND_PROGRESS, KIND_SLIDER) and node.value > 0:
            fill_w = max(1, int((w - bw[1] - bw[3]) * node.value / 100))
            fill_color = rgb565_to_qcolor(node.press_color) if node.press_color else QColor(0, 128, 0)
            fill_rect = QRectF(bw[3], bw[0], fill_w, h - bw[0] - bw[2])
            if r > 0:
                path = QPainterPath()
                path.addRoundedRect(fill_rect, r, r)
                painter.fillPath(path, QBrush(fill_color))
            else:
                painter.fillRect(fill_rect, fill_color)
            # Slider thumb
            if node.kind == KIND_SLIDER and node.border_color != 0:
                thumb_x = bw[3] + fill_w
                thumb_r = (h - bw[0] - bw[2]) // 2
                painter.setBrush(QBrush(rgb565_to_qcolor(node.border_color)))
                painter.setPen(Qt.NoPen)
                painter.drawEllipse(QPointF(thumb_x, h / 2), thumb_r, thumb_r)

        # Checkbox indicator
        if node.kind == KIND_CHECKBOX:
            m = 4
            box = QRectF(m, m, w - 2 * m, h - 2 * m)
            pen = QPen(rgb565_to_qcolor(node.text_color), 2)
            painter.setPen(pen)
            painter.setBrush(Qt.NoBrush)
            if r > 0:
                painter.drawRoundedRect(box, r // 2, r // 2)
            else:
                painter.drawRect(box)
            if node.checked:
                fc = rgb565_to_qcolor(node.press_color) if node.press_color else rgb565_to_qcolor(node.text_color)
                painter.setPen(QPen(fc, 3))
                painter.drawLine(QPointF(m + 4, h / 2), QPointF(w / 2 - 1, h - m - 4))
                painter.drawLine(QPointF(w / 2 - 1, h - m - 4), QPointF(w - m - 4, m + 4))

        # Radio indicator
        if node.kind == KIND_RADIO:
            cx, cy = w / 2, h / 2
            outer_r = min(w, h) / 2 - 4
            painter.setPen(QPen(rgb565_to_qcolor(node.text_color), 2))
            painter.setBrush(Qt.NoBrush)
            painter.drawEllipse(QPointF(cx, cy), outer_r, outer_r)
            if node.checked:
                fc = rgb565_to_qcolor(node.press_color) if node.press_color else rgb565_to_qcolor(node.text_color)
                painter.setPen(Qt.NoPen)
                painter.setBrush(QBrush(fc))
                painter.drawEllipse(QPointF(cx, cy), outer_r * 0.5, outer_r * 0.5)

        # Dropdown arrow
        if node.kind == KIND_DROPDOWN:
            color = rgb565_to_qcolor(node.text_color)
            painter.setPen(QPen(color, 2))
            ax = w - node.border[1] - node.padding[1] - 18
            ay = h / 2
            if node.checked:
                painter.drawLine(QPointF(ax, ay + 4), QPointF(ax + 6, ay - 3))
                painter.drawLine(QPointF(ax + 6, ay - 3), QPointF(ax + 12, ay + 4))
            else:
                painter.drawLine(QPointF(ax, ay - 3), QPointF(ax + 6, ay + 4))
                painter.drawLine(QPointF(ax + 6, ay + 4), QPointF(ax + 12, ay - 3))

        # Image preview
        if node.image_id and self.res_cache:
            pixmap = self.res_cache.get_image(node.image_id)
            if pixmap:
                pad_l = node.border[3] + node.padding[3]
                pad_t = node.border[0] + node.padding[0]
                pad_r = node.border[1] + node.padding[1]
                pad_b = node.border[2] + node.padding[2]
                target = QRectF(pad_l, pad_t, w - pad_l - pad_r, h - pad_t - pad_b)
                # Scale to fit while preserving aspect ratio
                pw, ph = pixmap.width(), pixmap.height()
                if pw > 0 and ph > 0:
                    scale = min(target.width() / pw, target.height() / ph)
                    sw, sh = pw * scale, ph * scale
                    dx = target.x() + (target.width() - sw) / 2
                    dy = target.y() + (target.height() - sh) / 2
                    painter.drawPixmap(QRectF(dx, dy, sw, sh), pixmap, QRectF(0, 0, pw, ph))

        # Text (Label, Button, or any widget with text)
        if node.text:
            pad_l = node.border[3] + node.padding[3]
            pad_r = node.border[1] + node.padding[1]
            pad_t = node.border[0] + node.padding[0]
            pad_b = node.border[2] + node.padding[2]
            text_color = rgb565_to_qcolor(node.text_color)
            gfx_font = self.res_cache.get_font(node.font_id) if self.res_cache else None
            if gfx_font:
                # Render with actual Adafruit GFX bitmap font
                tw = gfx_font.text_width(node.text)
                content_w = w - pad_l - pad_r - 4
                content_h = h - pad_t - pad_b
                # Horizontal alignment
                if node.text_align == 1:
                    tx = pad_l + 2 + (content_w - tw) // 2
                elif node.text_align == 2:
                    tx = pad_l + 2 + content_w - tw
                else:
                    tx = pad_l + 2
                # Vertical center — ty is the baseline Y.
                # Center on the actual visual bounds (ascent above + descent below baseline).
                ascent, descent = gfx_font.text_bounds(node.text)
                if ascent + descent == 0:
                    ascent, descent = gfx_font.line_height(), 0
                ty = pad_t + (content_h + ascent - descent) // 2
                painter.save()
                painter.setClipRect(QRectF(pad_l, pad_t, w - pad_l - pad_r, content_h))
                gfx_font.draw_text(painter, int(tx), int(ty), node.text, text_color)
                painter.restore()
            else:
                # Fallback: Qt font
                painter.setPen(QPen(text_color))
                font = QFont("Courier", 10)
                painter.setFont(font)
                flags = Qt.AlignVCenter
                if node.text_align == 0:
                    flags |= Qt.AlignLeft
                elif node.text_align == 1:
                    flags |= Qt.AlignHCenter
                else:
                    flags |= Qt.AlignRight
                text_rect = QRectF(pad_l + 2, pad_t, w - pad_l - pad_r - 4, h - pad_t - pad_b)
                painter.drawText(text_rect, flags, node.text)

        # Selection highlight
        if self.model.selected is node:
            painter.setPen(QPen(QColor(0, 150, 255), 2, Qt.DashLine))
            painter.setBrush(Qt.NoBrush)
            painter.drawRect(QRectF(0, 0, w, h))
        elif node in self.model.selection:
            painter.setPen(QPen(QColor(0, 150, 255), 1, Qt.DotLine))
            painter.setBrush(Qt.NoBrush)
            painter.drawRect(QRectF(0, 0, w, h))

        # Dim if not visible
        if not node.visible:
            painter.fillRect(QRectF(0, 0, w, h), QColor(128, 128, 128, 100))

    def mousePressEvent(self, event):
        if event.button() == Qt.LeftButton:
            if event.modifiers() & Qt.ControlModifier:
                self.model.select_toggle(self.node)
            else:
                if self.node not in self.model.selection:
                    self.model.select(self.node)
                else:
                    self.model.selected = self.node
                    self.model.selection_changed.emit()
                if self.node is not self.model.root:
                    self._dragging = True
                    self._drag_start = event.scenePos()
                    self._orig_loc = (self.node.loc_x, self.node.loc_y)
                    self._drag_orig_all = {
                        n: (n.loc_x, n.loc_y)
                        for n in self.model.selection
                        if n is not self.model.root
                    }
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event):
        if self._dragging:
            delta = event.scenePos() - self._drag_start
            dx, dy = int(delta.x()), int(delta.y())
            if len(self._drag_orig_all) > 1:
                for n, (ox, oy) in self._drag_orig_all.items():
                    n.loc_x = self.model.snap(ox + dx)
                    n.loc_y = self.model.snap(oy + dy)
            else:
                self.node.loc_x = self.model.snap(self._orig_loc[0] + dx)
                self.node.loc_y = self.model.snap(self._orig_loc[1] + dy)
            self.model.notify_changed()
            if self.scene():
                self.scene().update_guides(self.node)
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event):
        self._dragging = False
        if self.scene():
            self.scene().clear_guides()
        super().mouseReleaseEvent(event)


class HandleItem(QGraphicsRectItem):
    """Resize handle at a widget corner."""

    def __init__(self, corner, model):
        super().__init__(0, 0, HANDLE_SIZE, HANDLE_SIZE)
        self.corner = corner  # "nw", "ne", "sw", "se"
        self.model = model
        self.setBrush(QBrush(QColor(0, 150, 255)))
        self.setPen(QPen(QColor(255, 255, 255), 1))
        self.setFlag(QGraphicsItem.ItemIsMovable, False)
        self.setCursor(Qt.SizeFDiagCursor if corner in ("nw", "se") else Qt.SizeBDiagCursor)
        self._dragging = False
        self._drag_start = None
        self._orig = None

    def mousePressEvent(self, event):
        node = self.model.selected
        if node and event.button() == Qt.LeftButton:
            self._dragging = True
            self._drag_start = event.scenePos()
            self._orig = (node.loc_x, node.loc_y, node.size_w, node.size_h)
        event.accept()  # consume event — prevent WidgetItem underneath from dragging

    def mouseMoveEvent(self, event):
        if self._dragging and self._orig:
            node = self.model.selected
            if not node:
                return
            dx = int(event.scenePos().x() - self._drag_start.x())
            dy = int(event.scenePos().y() - self._drag_start.y())
            ox, oy, ow, oh = self._orig
            if self.corner == "se":
                node.size_w = max(10, ow + dx)
                node.size_h = max(10, oh + dy)
            elif self.corner == "sw":
                node.loc_x = ox + dx
                node.size_w = max(10, ow - dx)
                node.size_h = max(10, oh + dy)
            elif self.corner == "ne":
                node.loc_y = oy + dy
                node.size_w = max(10, ow + dx)
                node.size_h = max(10, oh - dy)
            elif self.corner == "nw":
                node.loc_x = ox + dx
                node.loc_y = oy + dy
                node.size_w = max(10, ow - dx)
                node.size_h = max(10, oh - dy)
            self.model.notify_changed()
        event.accept()

    def mouseReleaseEvent(self, event):
        self._dragging = False
        event.accept()


# ============================================================
# Canvas
# ============================================================

class DesignerScene(QGraphicsScene):
    def __init__(self, model):
        super().__init__()
        self.model = model
        self.res_cache = ResourceCache(model)
        self.items_map = {}
        self.handles = []
        self._h_guides = []  # y-coordinates of horizontal guide lines
        self._v_guides = []  # x-coordinates of vertical guide lines
        self._rb_origin = None   # QPointF where rubber-band drag started
        self._rb_rect = None     # QRectF of current rubber-band, or None
        self._update_scene_rect()
        model.tree_changed.connect(self.rebuild)
        model.changed.connect(self.refresh)
        model.selection_changed.connect(self.update_selection)
        self.rebuild()

    def _update_scene_rect(self):
        self.setSceneRect(-CANVAS_MARGIN, -CANVAS_MARGIN,
                          self.model.screen_w + 2 * CANVAS_MARGIN,
                          self.model.screen_h + 2 * CANVAS_MARGIN)

    def _screen_shape(self):
        return device_spec(self.model.device_id).get("shape", "rect")

    def drawBackground(self, painter, rect):
        # Dark gray background outside the screen
        painter.fillRect(rect, QColor(50, 50, 50))
        # Screen boundary — round screens get their outline in drawForeground,
        # over the corner mask, so skip the rectangular outline here.
        if self._screen_shape() != "round":
            painter.setPen(QPen(QColor(100, 100, 100), 1))
            painter.drawRect(QRectF(-1, -1, self.model.screen_w + 2, self.model.screen_h + 2))

    def drawForeground(self, painter, rect):
        # Round screen: mask the corners (outside the inscribed circle) so the
        # canvas reads as a circular display and overflowing widgets are clipped.
        if self._screen_shape() == "round":
            sw, sh = self.model.screen_w, self.model.screen_h
            screen_rect = QRectF(0, 0, sw, sh)
            corners = QPainterPath()
            corners.addRect(screen_rect)
            disc = QPainterPath()
            disc.addEllipse(screen_rect)
            painter.save()
            painter.setPen(Qt.NoPen)
            painter.fillPath(corners.subtracted(disc), QColor(50, 50, 50))
            # Circular screen outline
            painter.setBrush(Qt.NoBrush)
            painter.setPen(QPen(QColor(100, 100, 100), 1))
            painter.drawEllipse(screen_rect)
            painter.restore()

        # Smart guide lines
        if self._h_guides or self._v_guides:
            pen = QPen(QColor(0, 210, 255), 0)
            pen.setStyle(Qt.DashLine)
            painter.setPen(pen)
            for y in self._h_guides:
                painter.drawLine(QPointF(rect.left(), y), QPointF(rect.right(), y))
            for x in self._v_guides:
                painter.drawLine(QPointF(x, rect.top()), QPointF(x, rect.bottom()))

        # Rubber-band selection rectangle
        if self._rb_rect and self._rb_rect.width() > 2 and self._rb_rect.height() > 2:
            painter.save()
            painter.setPen(QPen(QColor(0, 150, 255), 0, Qt.DashLine))
            painter.setBrush(QBrush(QColor(0, 120, 255, 40)))
            painter.drawRect(self._rb_rect)
            painter.restore()

    def update_guides(self, drag_node):
        """Compute snap-assist guide lines while dragging drag_node."""
        ax, ay = drag_node.abs_pos()
        w, h = drag_node.size_w, drag_node.size_h
        cx, cy = ax + w / 2, ay + h / 2
        rx, by = ax + w, ay + h

        drag_h = [ay, cy, by]
        drag_v = [ax, cx, rx]

        sw, sh = self.model.screen_w, self.model.screen_h
        src_h = [0, sh / 2, sh]
        src_v = [0, sw / 2, sw]
        for node in self.model.widgets:
            if node is drag_node or node is self.model.root:
                continue
            nx, ny = node.abs_pos()
            nw, nh = node.size_w, node.size_h
            src_h.extend([ny, ny + nh / 2, ny + nh])
            src_v.extend([nx, nx + nw / 2, nx + nw])

        self._h_guides = []
        self._v_guides = []
        for dy in drag_h:
            for sy in src_h:
                if abs(dy - sy) <= GUIDE_SNAP_DIST:
                    if sy not in self._h_guides:
                        self._h_guides.append(sy)
                    break
        for dx in drag_v:
            for sx in src_v:
                if abs(dx - sx) <= GUIDE_SNAP_DIST:
                    if sx not in self._v_guides:
                        self._v_guides.append(sx)
                    break
        self.update()

    def clear_guides(self):
        self._h_guides = []
        self._v_guides = []
        self.update()

    def rebuild(self):
        self._update_scene_rect()
        for item in list(self.items_map.values()):
            self.removeItem(item)
        self.items_map.clear()
        self._clear_handles()

        dfs = self.model.dfs_order()
        for i, node in enumerate(dfs):
            item = WidgetItem(node, self.model, self.res_cache)
            item.setZValue(i)
            ax, ay = node.abs_pos()
            item.setPos(ax, ay)
            self.addItem(item)
            self.items_map[node] = item
        self.update_selection()

    def refresh(self):
        for node, item in self.items_map.items():
            ax, ay = node.abs_pos()
            item.setPos(ax, ay)
            item.prepareGeometryChange()
            item.update()
        self._position_handles()

    def update_selection(self):
        self._clear_handles()
        sel = self.model.selected
        if sel and sel is not self.model.root:
            for corner in ("nw", "ne", "sw", "se"):
                h = HandleItem(corner, self.model)
                h.setZValue(10000)
                self.addItem(h)
                self.handles.append(h)
            self._position_handles()
        for node, item in self.items_map.items():
            item.update()

    def _position_handles(self):
        sel = self.model.selected
        if not sel or not self.handles:
            return
        ax, ay = sel.abs_pos()
        w, h = sel.size_w, sel.size_h
        hs = HANDLE_SIZE
        positions = {
            "nw": (ax - hs // 2, ay - hs // 2),
            "ne": (ax + w - hs // 2, ay - hs // 2),
            "sw": (ax - hs // 2, ay + h - hs // 2),
            "se": (ax + w - hs // 2, ay + h - hs // 2),
        }
        for handle in self.handles:
            px, py = positions[handle.corner]
            handle.setPos(px, py)

    def _clear_handles(self):
        for h in self.handles:
            self.removeItem(h)
        self.handles.clear()

    def _hit_item(self, scene_pos):
        xform = self.views()[0].transform() if self.views() else __import__('PySide6.QtGui', fromlist=['QTransform']).QTransform()
        return self.itemAt(scene_pos, xform)

    def mousePressEvent(self, event):
        if event.button() == Qt.LeftButton:
            item = self._hit_item(event.scenePos())
            is_background = item is None or (
                isinstance(item, WidgetItem) and item.node is self.model.root)
            if is_background:
                # Start rubber-band selection on empty canvas / root background
                if not (event.modifiers() & Qt.ControlModifier):
                    self.model.select(None)
                self._rb_origin = event.scenePos()
                self._rb_rect = QRectF(self._rb_origin, self._rb_origin)
                event.accept()
                return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event):
        if self._rb_origin is not None:
            pos = event.scenePos()
            self._rb_rect = QRectF(self._rb_origin, pos).normalized()
            self.update()
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event):
        if event.button() == Qt.LeftButton and self._rb_origin is not None:
            rb = self._rb_rect
            self._rb_origin = None
            self._rb_rect = None
            self.update()

            if rb and (rb.width() > 4 or rb.height() > 4):
                add = bool(event.modifiers() & Qt.ControlModifier)
                if not add:
                    self.model.selection = set()
                last = None
                for node in self.model.widgets:
                    if node is self.model.root:
                        continue
                    ax, ay = node.abs_pos()
                    node_rect = QRectF(ax, ay, node.size_w, node.size_h)
                    if rb.intersects(node_rect):
                        self.model.selection.add(node)
                        last = node
                if last is not None:
                    self.model.selected = last
                elif not add:
                    self.model.selected = None
                self.model.selection_changed.emit()

            event.accept()
            return
        super().mouseReleaseEvent(event)


class CanvasView(QGraphicsView):
    MIN_ZOOM = 0.4
    MAX_ZOOM = 5.0

    def __init__(self, scene):
        super().__init__(scene)
        self.setRenderHint(QPainter.Antialiasing)
        self.setDragMode(QGraphicsView.NoDrag)
        self.setMinimumSize(400, 300)
        self._zoom = 1.0

    def resizeEvent(self, event):
        super().resizeEvent(event)
        self.fitInView(self.scene().sceneRect(), Qt.KeepAspectRatio)
        # Sync _zoom with the actual transform after fit
        self._zoom = self.transform().m11()

    def wheelEvent(self, event):
        factor = 1.15 if event.angleDelta().y() > 0 else 1 / 1.15
        new_zoom = self._zoom * factor
        if new_zoom < self.MIN_ZOOM or new_zoom > self.MAX_ZOOM:
            return
        self._zoom = new_zoom
        self.scale(factor, factor)


# ============================================================
# Tree panel
# ============================================================

class TreePanel(QTreeWidget):
    def __init__(self, model):
        super().__init__()
        self.model = model
        self.setHeaderLabels(["Widget"])
        self.setMinimumWidth(200)
        self.itemClicked.connect(self._on_click)
        model.tree_changed.connect(self.rebuild)
        model.selection_changed.connect(self._sync_selection)
        self.rebuild()

    def rebuild(self):
        self.blockSignals(True)
        self.clear()
        node_items = {}

        def add_node(node, parent_item=None):
            if parent_item is None:
                item = QTreeWidgetItem(self, [node.name])
            else:
                item = QTreeWidgetItem(parent_item, [node.name])
            icon = designer_icon(KIND_ICON_NAMES.get(node.kind, ""))
            if icon:
                item.setIcon(0, icon)
            item.setToolTip(0, f"{kind_name(node.kind)}  {node.size_w}×{node.size_h}")
            item.setData(0, Qt.UserRole, node)
            item.setExpanded(True)
            node_items[node] = item
            for child in node.children:
                add_node(child, item)

        add_node(self.model.root)
        self.blockSignals(False)
        self._sync_selection()

    def _on_click(self, item, col):
        node = item.data(0, Qt.UserRole)
        if node:
            self.model.select(node)

    def _sync_selection(self):
        self.blockSignals(True)
        sel = self.model.selected
        self.clearSelection()
        if sel:
            it = self._find_item(self.invisibleRootItem(), sel)
            if it:
                it.setSelected(True)
                self.scrollToItem(it)
        self.blockSignals(False)

    def _find_item(self, parent, node):
        for i in range(parent.childCount()):
            child = parent.child(i)
            if child.data(0, Qt.UserRole) is node:
                return child
            found = self._find_item(child, node)
            if found:
                return found
        return None


# ============================================================
# Property editor
# ============================================================

class ColorButton(QPushButton):
    """Button that shows a color swatch and opens a color picker."""
    color_changed = Signal(int)

    def __init__(self, value=0, model=None):
        super().__init__()
        self.model = model
        self._value = value
        self.setFixedSize(60, 24)
        self._update_style()
        self.clicked.connect(self._pick)

    def value(self):
        return self._value

    def set_value(self, v):
        self._value = v
        self._update_style()

    def _update_style(self):
        qc = rgb565_to_qcolor(self._value)
        self.setStyleSheet(f"background-color: {qc.name()}; border: 1px solid #888;")
        self.setToolTip(f"0x{self._value:04X}")

    def _pick(self):
        qc = rgb565_to_qcolor(self._value)
        title = "Pick Color"
        if self.model and device_spec(self.model.device_id)["color_model"] == "4-bit grayscale":
            title = "Pick Color (quantized to 16 grayscale levels)"
        color = QColorDialog.getColor(qc, self, title)
        if color.isValid():
            value = qcolor_to_rgb565(color)
            if self.model:
                value = quantize_color_for_device(value, self.model.device_id)
            self._value = value
            self._update_style()
            self.color_changed.emit(self._value)


class CallbackLineEdit(QLineEdit):
    """QLineEdit that auto-creates a callback function on double-click."""

    def __init__(self, attr, prop_editor, parent=None):
        super().__init__(parent)
        self._attr = attr
        self._prop_editor = prop_editor
        self.setPlaceholderText("double-click to create")

    def mouseDoubleClickEvent(self, event):
        if not self.text():
            self._prop_editor._auto_bind_callback(self._attr)
        super().mouseDoubleClickEvent(event)


class EdgesEditor(QWidget):
    """Four spinboxes for T, R, B, L edges."""
    values_changed = Signal(list)

    def __init__(self):
        super().__init__()
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(2)
        self._spins = []
        for label in ("T", "R", "B", "L"):
            sp = QSpinBox()
            sp.setRange(0, 99)
            sp.setFixedWidth(44)
            sp.setToolTip(label)
            sp.valueChanged.connect(self._on_change)
            layout.addWidget(sp)
            self._spins.append(sp)

    def values(self):
        return [s.value() for s in self._spins]

    def set_values(self, vals):
        for s, v in zip(self._spins, vals):
            s.blockSignals(True)
            s.setValue(v)
            s.blockSignals(False)

    def _on_change(self):
        self.values_changed.emit(self.values())


class PropertyEditor(QScrollArea):
    """Widget property editor panel."""

    def __init__(self, model):
        super().__init__()
        self.model = model
        self._updating = False
        self.setWidgetResizable(True)
        self.setMinimumWidth(280)
        self.setMaximumWidth(340)

        container = QWidget()
        self._layout = QVBoxLayout(container)
        self._layout.setSpacing(4)
        self._layout.setContentsMargins(4, 4, 4, 4)
        self.setWidget(container)

        self._editors = {}
        self._build_ui()

        model.selection_changed.connect(self.load_widget)
        model.changed.connect(self._refresh_title)
        model.changed.connect(self._refresh_resource_combos)

    def _build_ui(self):
        # Identity
        g = self._group("Identity")
        self._add_str(g, "name", "Name")
        self._add_combo(g, "kind", "Kind", KIND_NAMES)
        self._add_combo(g, "parent_w", "Parent", [])

        # Geometry
        g = self._group("Geometry")
        self._add_int(g, "loc_x", "X", -9999, 9999)
        self._add_int(g, "loc_y", "Y", -9999, 9999)
        self._add_int(g, "size_w", "Width", 1, 9999)
        self._add_int(g, "size_h", "Height", 1, 9999)

        # Appearance
        g = self._group("Appearance")
        self._add_color(g, "bg_color", "Background")
        self._add_color(g, "border_color", "Border Color")
        self._add_int(g, "border_radius", "Radius", 0, 999)
        self._add_color(g, "text_color", "Text Color")
        self._add_color(g, "press_color", "Press Color")
        # Background opacity — only shown for devices with caps.alpha
        self._add_int(g, "alpha", "Alpha", 0, 255)
        self._alpha_layout = g

        # Box model
        g = self._group("Box Model")
        self._add_edges(g, "border", "Border")
        self._add_edges(g, "margin", "Margin")
        self._add_edges(g, "padding", "Padding")

        # Text
        g = self._group("Text")
        self._add_str(g, "text", "Text")
        self._add_resource_combo(g, "font_id", "Font", "fonts")
        self._add_combo(g, "text_align", "Align", ["Left", "Center", "Right"])

        # State
        g = self._group("State")
        self._add_bool(g, "visible", "Visible")
        self._add_bool(g, "enabled", "Enabled")
        self._add_bool(g, "clickable", "Clickable")
        self._add_int(g, "value", "Value", 0, 100)
        self._add_bool(g, "checked", "Checked")
        self._add_bool(g, "multi_line", "Multi-line")
        self._add_resource_combo(g, "image_id", "Image", "images")

        # Callbacks (double-click to auto-create handler)
        g = self._group("Callbacks")
        self._add_callback(g, "on_click", "on_click")
        self._add_callback(g, "on_tap", "on_tap")
        self._add_callback(g, "on_paint", "on_paint")

        self._layout.addStretch()

    def _group(self, title):
        grp = QGroupBox(title)
        layout = QFormLayout(grp)
        layout.setSpacing(3)
        layout.setContentsMargins(6, 10, 6, 4)
        self._layout.addWidget(grp)
        return layout

    def _add_int(self, layout, attr, label, lo, hi):
        sp = QSpinBox()
        sp.setRange(lo, hi)
        sp.valueChanged.connect(lambda v, a=attr: self._set_attr(a, v))
        layout.addRow(label, sp)
        self._editors[attr] = sp

    def _add_str(self, layout, attr, label):
        le = QLineEdit()
        le.editingFinished.connect(lambda a=attr, e=le: self._set_attr(a, e.text()))
        layout.addRow(label, le)
        self._editors[attr] = le

    def _add_bool(self, layout, attr, label):
        cb = QCheckBox()
        cb.stateChanged.connect(lambda v, a=attr: self._set_attr(a, bool(v)))
        layout.addRow(label, cb)
        self._editors[attr] = cb

    def _add_combo(self, layout, attr, label, items):
        cb = QComboBox()
        cb.addItems(items)
        cb.currentIndexChanged.connect(lambda v, a=attr: self._set_attr(a, v))
        layout.addRow(label, cb)
        self._editors[attr] = cb

    def _add_color(self, layout, attr, label):
        row = QWidget()
        rl = QHBoxLayout(row)
        rl.setContentsMargins(0, 0, 0, 0)
        rl.setSpacing(4)
        btn = ColorButton(model=self.model)
        btn.color_changed.connect(lambda v, a=attr: self._set_attr(a, v))
        hex_label = QLabel("0x0000")
        hex_label.setFixedWidth(52)
        rl.addWidget(btn)
        rl.addWidget(hex_label)
        rl.addStretch()
        layout.addRow(label, row)
        self._editors[attr] = (btn, hex_label)

    def _add_edges(self, layout, attr, label):
        ed = EdgesEditor()
        ed.values_changed.connect(lambda v, a=attr: self._set_attr(a, v))
        layout.addRow(label, ed)
        self._editors[attr] = ed

    def _add_callback(self, layout, attr, label):
        """Add a callback QLineEdit that auto-binds on double-click."""
        le = CallbackLineEdit(attr, self)
        le.editingFinished.connect(lambda a=attr, e=le: self._set_attr(a, e.text()))
        layout.addRow(label, le)
        self._editors[attr] = le

    def _auto_bind_callback(self, attr):
        """Auto-create a callback function in main.fl for the selected widget."""
        node = self.model.selected
        if not node:
            return

        # Generate function name: widget_name + event suffix
        suffix_map = {"on_click": "click", "on_tap": "tap", "on_paint": "paint"}
        suffix = suffix_map.get(attr, attr)
        func_name = f"{node.name}_{suffix}"

        # Set the property
        setattr(node, attr, func_name)
        self._set_editor(attr, func_name)
        self.model.notify_changed()

        # Generate function stub based on callback type
        if attr == "on_paint":
            stub = f"\nfn {func_name}(widget_id) {{\n    // Custom paint for {node.name}\n}}\n"
        elif attr == "on_tap":
            stub = f"\nfn {func_name}(widget_id) {{\n    // Tap handler for {node.name}\n}}\n"
        else:
            stub = f"\nfn {func_name}(widget_id) {{\n    // Click handler for {node.name}\n}}\n"

        # Insert into main.fl if function doesn't already exist
        if f"fn {func_name}(" not in self.model.main_fl:
            self.model.main_fl = self.model.main_fl.rstrip() + "\n" + stub

    def _add_resource_combo(self, layout, attr, label, resource_key):
        """Add a combobox for selecting a resource (font or image) by name."""
        cb = QComboBox()
        cb.setProperty("resource_key", resource_key)
        cb.setProperty("attr_name", attr)
        cb.currentIndexChanged.connect(lambda idx, a=attr, c=cb: self._set_attr(a, c.currentData() if c.currentData() is not None else 0))
        layout.addRow(label, cb)
        self._editors[attr] = cb

    def _refresh_resource_combos(self):
        """Rebuild font_id and image_id combo items from model resources."""
        for attr in ("font_id", "image_id"):
            editor = self._editors.get(attr)
            if not isinstance(editor, QComboBox) or editor.property("resource_key") is None:
                continue
            rkey = editor.property("resource_key")
            id_field = attr  # "font_id" or "image_id"

            editor.blockSignals(True)
            prev_data = editor.currentData()
            editor.clear()

            # First item: "None (0)" for no resource
            if attr == "font_id":
                editor.addItem("Embedded (0)", 0)
            else:
                editor.addItem("None (0)", 0)

            resources = getattr(self.model, rkey, [])
            for res in resources:
                rid = res.get(id_field, 0)
                name = res.get("name", f"id {rid}")
                editor.addItem(f"{name} ({rid})", rid)

            # Restore selection
            if prev_data is not None:
                idx = editor.findData(prev_data)
                if idx >= 0:
                    editor.setCurrentIndex(idx)
            editor.blockSignals(False)

    def _set_attr(self, attr, value):
        if self._updating:
            return
        node = self.model.selected
        if not node:
            return

        if attr == "name":
            if not value or not value.isidentifier():
                self.load_widget()
                return
            if any(w.name == value and w is not node for w in self.model.widgets):
                self.load_widget()
                return
            node.name = value
            self.model.tree_changed.emit()
        elif attr == "kind":
            node.kind = KIND_VALUES[value] if 0 <= value < len(KIND_VALUES) else KIND_BASE
            self.model.tree_changed.emit()
        elif attr == "parent_w":
            combo = self._editors["parent_w"]
            parent_name = combo.currentText()
            new_parent = self.model.find_by_name(parent_name)
            if new_parent and new_parent is not node.parent:
                self.model.reparent(node, new_parent)
        elif attr in ("border", "margin", "padding"):
            setattr(node, attr, list(value))
            self.model.notify_changed()
        elif attr in ("bg_color", "border_color", "text_color", "press_color"):
            setattr(node, attr, quantize_color_for_device(int(value), self.model.device_id))
            self.model.notify_changed()
        else:
            setattr(node, attr, value)
            self.model.notify_changed()

    def load_widget(self):
        self._updating = True
        node = self.model.selected
        enabled = node is not None

        for key, editor in self._editors.items():
            if isinstance(editor, tuple):
                editor[0].setEnabled(enabled)
            elif isinstance(editor, QWidget):
                editor.setEnabled(enabled)

        if not node:
            self._updating = False
            return

        is_root = node is self.model.root

        self._set_editor("name", node.name)
        self._editors["name"].setEnabled(not is_root)

        self._set_editor("kind", node.kind)
        self._editors["kind"].setEnabled(not is_root)

        # Parent combo
        combo = self._editors["parent_w"]
        combo.blockSignals(True)
        combo.clear()
        desc = node.descendants()
        desc.add(node)
        available = [w.name for w in self.model.widgets if w not in desc]
        combo.addItems(available)
        if node.parent:
            idx = combo.findText(node.parent.name)
            if idx >= 0:
                combo.setCurrentIndex(idx)
        combo.setEnabled(not is_root)
        combo.blockSignals(False)

        self._set_editor("loc_x", node.loc_x)
        self._set_editor("loc_y", node.loc_y)
        self._set_editor("size_w", node.size_w)
        self._set_editor("size_h", node.size_h)
        self._editors["loc_x"].setEnabled(not is_root)
        self._editors["loc_y"].setEnabled(not is_root)
        self._editors["size_w"].setEnabled(not is_root)
        self._editors["size_h"].setEnabled(not is_root)

        self._set_editor("bg_color", node.bg_color)
        self._set_editor("border_color", node.border_color)
        self._set_editor("border_radius", node.border_radius)
        self._set_editor("text_color", node.text_color)
        self._set_editor("press_color", node.press_color)
        self._set_editor("alpha", node.alpha)
        # Alpha blending requires framebuffer readback — hide the editor on
        # devices that cannot do it (see devices/*.json caps.alpha).
        has_alpha = bool(device_spec(self.model.device_id)["caps"].get("alpha"))
        alpha_editor = self._editors["alpha"]
        alpha_editor.setVisible(has_alpha)
        alpha_label = self._alpha_layout.labelForField(alpha_editor)
        if alpha_label is not None:
            alpha_label.setVisible(has_alpha)

        self._set_editor("border", node.border)
        self._set_editor("margin", node.margin)
        self._set_editor("padding", node.padding)

        self._refresh_resource_combos()

        self._set_editor("text", node.text)
        self._set_editor("font_id", node.font_id)
        self._set_editor("text_align", node.text_align)

        self._set_editor("visible", node.visible)
        self._set_editor("enabled", node.enabled)
        self._set_editor("clickable", node.clickable)
        self._set_editor("value", node.value)
        self._set_editor("checked", node.checked)
        self._set_editor("multi_line", node.multi_line)
        self._set_editor("image_id", node.image_id)

        self._set_editor("on_click", node.on_click)
        self._set_editor("on_tap", node.on_tap)
        self._set_editor("on_paint", node.on_paint)

        self._updating = False

    def _set_editor(self, attr, value):
        editor = self._editors.get(attr)
        if editor is None:
            return
        if isinstance(editor, tuple):
            btn, lbl = editor
            btn.set_value(value)
            lbl.setText(f"0x{value:04X}")
        elif isinstance(editor, QSpinBox):
            editor.blockSignals(True)
            editor.setValue(value)
            editor.blockSignals(False)
        elif isinstance(editor, QLineEdit):
            editor.blockSignals(True)
            editor.setText(str(value))
            editor.blockSignals(False)
        elif isinstance(editor, QCheckBox):
            editor.blockSignals(True)
            editor.setChecked(bool(value))
            editor.blockSignals(False)
        elif isinstance(editor, QComboBox):
            editor.blockSignals(True)
            if editor.property("resource_key") is not None:
                # Resource combo: find by data (resource ID)
                idx = editor.findData(int(value))
                editor.setCurrentIndex(max(0, idx))
            elif editor is self._editors.get("kind"):
                idx = KIND_VALUES.index(int(value)) if int(value) in KIND_VALUES else 0
                editor.setCurrentIndex(idx)
            else:
                editor.setCurrentIndex(int(value))
            editor.blockSignals(False)
        elif isinstance(editor, EdgesEditor):
            editor.set_values(value)

    def _refresh_title(self):
        if self._updating:
            return
        node = self.model.selected
        if node:
            self._updating = True
            self._set_editor("loc_x", node.loc_x)
            self._set_editor("loc_y", node.loc_y)
            self._set_editor("size_w", node.size_w)
            self._set_editor("size_h", node.size_h)
            self._updating = False


# ============================================================
# Add-font dialog
# ============================================================

class AddFontDialog(QDialog):
    """Dialog to generate one or more sparse font binaries from a TrueType file."""

    DEFAULT_RANGES = "32-126"

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Add Font from TrueType")
        self.setMinimumWidth(500)
        layout = QFormLayout(self)
        layout.setSpacing(10)
        layout.setContentsMargins(12, 12, 12, 12)

        # Font file row
        file_row = QWidget()
        fl = QHBoxLayout(file_row)
        fl.setContentsMargins(0, 0, 0, 0)
        fl.setSpacing(4)
        self._file_edit = QLineEdit()
        self._file_edit.setPlaceholderText("path/to/font.ttf")
        browse_btn = QPushButton("Browse…")
        set_icon(browse_btn, "fa5s.folder-open", QStyle.SP_DialogOpenButton)
        browse_btn.clicked.connect(self._browse)
        fl.addWidget(self._file_edit, 1)
        fl.addWidget(browse_btn)
        layout.addRow("Font file:", file_row)

        # Sizes
        self._sizes_edit = QLineEdit()
        self._sizes_edit.setPlaceholderText("e.g. 18,20,24,72")
        self._sizes_edit.setText("18,20,24")
        layout.addRow("Sizes (pt):", self._sizes_edit)

        # Codepoint ranges
        self._ranges_edit = QLineEdit()
        self._ranges_edit.setPlaceholderText(
            "e.g. 32-127,0xC7,0xD6,0x011E-0x011F")
        self._ranges_edit.setText(self.DEFAULT_RANGES)
        layout.addRow("Codepoint ranges:", self._ranges_edit)

        # Anti-aliasing (bits per pixel)
        self._bpp_combo = QComboBox()
        self._bpp_combo.addItem("Anti-aliased (2bpp, 4 gray levels)", 2)
        self._bpp_combo.addItem("Monochrome (1bpp)", 1)
        layout.addRow("Rendering:", self._bpp_combo)

        hint = QLabel(
            "Sizes: comma-separated point sizes (each becomes a separate font entry).\n"
            "Ranges: decimal, hex (0x011E), or char ('A'), dash for inclusive ranges.\n"
            "Rendering: anti-aliased is smoother; monochrome is half the size.")
        hint.setWordWrap(True)
        hint.setStyleSheet("color: gray; font-size: 11px;")
        layout.addRow("", hint)

        btns = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        btns.button(QDialogButtonBox.Ok).setText("Generate & Add")
        btns.accepted.connect(self._validate_and_accept)
        btns.rejected.connect(self.reject)
        layout.addRow(btns)

    def _browse(self):
        path, _ = QFileDialog.getOpenFileName(
            self, "Select TrueType Font", "",
            "TrueType Fonts (*.ttf *.TTF *.otf *.OTF);;All Files (*)")
        if path:
            self._file_edit.setText(path)

    def _validate_and_accept(self):
        if not self._file_edit.text().strip():
            QMessageBox.warning(self, "Missing file", "Please select a .ttf font file.")
            return
        if not os.path.isfile(self._file_edit.text().strip()):
            QMessageBox.warning(self, "File not found",
                                f"File not found:\n{self._file_edit.text().strip()}")
            return
        try:
            sizes = self._parse_sizes()
        except ValueError as e:
            QMessageBox.warning(self, "Invalid sizes", str(e))
            return
        if not sizes:
            QMessageBox.warning(self, "No sizes", "Enter at least one point size.")
            return
        try:
            self._parse_ranges()
        except Exception as e:
            QMessageBox.warning(self, "Invalid ranges", str(e))
            return
        self.accept()

    def _parse_sizes(self):
        sizes = []
        for tok in self._sizes_edit.text().split(","):
            tok = tok.strip()
            if not tok:
                continue
            try:
                v = int(tok)
            except ValueError:
                raise ValueError(f"Not a valid integer: {tok!r}")
            if v < 4 or v > 256:
                raise ValueError(f"Size {v} is out of range (4–256).")
            sizes.append(v)
        return sizes

    def _parse_ranges(self):
        # Import lazily — only needed here and in the conversion step.
        tools_dir = os.path.dirname(os.path.abspath(__file__))
        if tools_dir not in sys.path:
            sys.path.insert(0, tools_dir)
        from ferrite_font_converter import parse_ranges
        return parse_ranges(self._ranges_edit.text().strip() or self.DEFAULT_RANGES)

    # Public accessors used by ResourcePanel after accept()

    def font_file(self):
        return self._file_edit.text().strip()

    def sizes(self):
        return self._parse_sizes()

    def ranges_spec(self):
        return self._ranges_edit.text().strip() or self.DEFAULT_RANGES

    def bpp(self):
        return self._bpp_combo.currentData()


# ============================================================
# Resource panel
# ============================================================

# ============================================================
# SVG icon picker dialog
# ============================================================

class SvgIconPickerDialog(QDialog):
    """Browse a folder of SVG icons, filter by name, preview thumbnails."""

    _MAX_VISIBLE = 300

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Browse SVG Icons")
        self.resize(760, 540)
        self._folder = QSettings().value(SVG_ICON_FOLDER_KEY, SVG_ICON_FOLDER_DEFAULT)
        self._all_items = []   # list of (display_name, full_path)
        self._build_ui()
        self._scan_folder()

    def _build_ui(self):
        vbox = QVBoxLayout(self)

        # Folder row
        fr = QHBoxLayout()
        fr.addWidget(QLabel("Folder:"))
        self._folder_edit = QLineEdit(self._folder)
        self._folder_edit.setReadOnly(True)
        browse_btn = QPushButton("…")
        browse_btn.setFixedWidth(32)
        browse_btn.clicked.connect(self._browse_folder)
        fr.addWidget(self._folder_edit)
        fr.addWidget(browse_btn)
        vbox.addLayout(fr)

        # Search + size row
        sr = QHBoxLayout()
        sr.addWidget(QLabel("Search:"))
        self._search = QLineEdit()
        self._search.setPlaceholderText("type to filter…")
        self._search.setClearButtonEnabled(True)
        self._search.textChanged.connect(self._apply_filter)
        sr.addWidget(self._search)
        sr.addWidget(QLabel("Size:"))
        self._size_combo = QComboBox()
        for px in (16, 24, 32, 48, 64):
            self._size_combo.addItem(f"{px}px", px)
        self._size_combo.setCurrentIndex(2)  # default 32 px
        sr.addWidget(self._size_combo)
        vbox.addLayout(sr)

        self._count_lbl = QLabel()
        vbox.addWidget(self._count_lbl)

        # Icon grid
        self._grid = QListWidget()
        self._grid.setViewMode(QListWidget.IconMode)
        self._grid.setIconSize(QSize(48, 48))
        self._grid.setResizeMode(QListWidget.Adjust)
        self._grid.setWordWrap(True)
        self._grid.setSpacing(4)
        self._grid.setUniformItemSizes(True)
        self._grid.itemDoubleClicked.connect(self.accept)
        vbox.addWidget(self._grid)

        btns = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        btns.accepted.connect(self.accept)
        btns.rejected.connect(self.reject)
        vbox.addWidget(btns)

    def _browse_folder(self):
        folder = QFileDialog.getExistingDirectory(self, "SVG Icon Folder", self._folder)
        if folder:
            self._folder = folder
            self._folder_edit.setText(folder)
            QSettings().setValue(SVG_ICON_FOLDER_KEY, folder)
            self._scan_folder()

    def _scan_folder(self):
        self._all_items = []
        if os.path.isdir(self._folder):
            for fn in sorted(os.listdir(self._folder)):
                if fn.lower().endswith(".svg"):
                    self._all_items.append((os.path.splitext(fn)[0],
                                            os.path.join(self._folder, fn)))
        self._apply_filter(self._search.text())

    def _apply_filter(self, query):
        q = query.strip().lower()
        if q:
            words = q.split()
            filtered = [(d, p) for d, p in self._all_items
                        if all(w in d.lower() for w in words)]
        else:
            filtered = self._all_items

        total = len(filtered)
        capped = filtered[:self._MAX_VISIBLE]
        suffix = f"  (showing first {self._MAX_VISIBLE})" if total > self._MAX_VISIBLE else ""
        self._count_lbl.setText(f"{total} icons{suffix}")

        self._grid.clear()
        for display, path in capped:
            item = QListWidgetItem(display)
            item.setData(Qt.UserRole, path)
            item.setIcon(QIcon(path))   # Qt renders SVG lazily via QtSvg
            item.setSizeHint(QSize(84, 72))
            self._grid.addItem(item)

    def selected_path(self):
        item = self._grid.currentItem()
        return item.data(Qt.UserRole) if item else None

    def selected_size(self):
        return self._size_combo.currentData()

    @staticmethod
    def render_svg_to_png(path, size):
        """Render an SVG file to PNG bytes at *size* × *size* pixels."""
        try:
            from PySide6.QtSvg import QSvgRenderer
        except ImportError:
            raise RuntimeError("PySide6.QtSvg is required: pip install PySide6")
        renderer = QSvgRenderer(path)
        img = QImage(size, size, QImage.Format_ARGB32)
        img.fill(0)  # transparent
        painter = QPainter(img)
        renderer.render(painter)
        painter.end()
        buf = QBuffer()
        buf.open(QBuffer.WriteOnly)
        img.save(buf, "PNG")
        return bytes(buf.data())


# ============================================================
# Resource panel
# ============================================================

class ResourcePanel(QWidget):
    """Panel for managing project resources (fonts, images, programs)."""

    def __init__(self, model):
        super().__init__()
        self.model = model
        layout = QVBoxLayout(self)
        layout.setSpacing(4)
        layout.setContentsMargins(4, 4, 4, 4)

        # Fonts
        layout.addWidget(QLabel("Fonts"))
        self.font_list = QTreeWidget()
        self.font_list.setHeaderLabels(["Name", "ID", "Size"])
        self.font_list.setMaximumHeight(100)
        self.font_list.setRootIsDecorated(False)
        layout.addWidget(self.font_list)
        fb = QHBoxLayout()
        add_font_btn = QPushButton("+ Font")
        set_icon(add_font_btn, "fa5s.font", QStyle.SP_FileIcon)
        add_font_btn.clicked.connect(self._add_font)
        rm_font_btn = QPushButton("- Font")
        set_icon(rm_font_btn, "fa5s.trash", QStyle.SP_TrashIcon)
        rm_font_btn.clicked.connect(self._rm_font)
        fb.addWidget(add_font_btn)
        fb.addWidget(rm_font_btn)
        fb.addStretch()
        layout.addLayout(fb)

        # Images
        layout.addWidget(QLabel("Images"))
        self.image_list = QTreeWidget()
        self.image_list.setHeaderLabels(["Name", "ID", "Source"])
        self.image_list.setMaximumHeight(100)
        self.image_list.setRootIsDecorated(False)
        layout.addWidget(self.image_list)
        ib = QHBoxLayout()
        add_img_btn = QPushButton("+ Image")
        set_icon(add_img_btn, "fa5s.image", QStyle.SP_FileIcon)
        add_img_btn.clicked.connect(self._add_image)
        svg_btn = QPushButton("Browse Icons…")
        set_icon(svg_btn, "fa5s.icons", QStyle.SP_FileDialogListView)
        svg_btn.clicked.connect(self._add_image_from_svg)
        rm_img_btn = QPushButton("- Image")
        set_icon(rm_img_btn, "fa5s.trash", QStyle.SP_TrashIcon)
        rm_img_btn.clicked.connect(self._rm_image)
        ib.addWidget(add_img_btn)
        ib.addWidget(svg_btn)
        ib.addWidget(rm_img_btn)
        ib.addStretch()
        layout.addLayout(ib)

        # Extra programs
        layout.addWidget(QLabel("Extra Programs"))
        self.prog_list = QTreeWidget()
        self.prog_list.setHeaderLabels(["Name", "Source", "Exec"])
        self.prog_list.setMaximumHeight(80)
        self.prog_list.setRootIsDecorated(False)
        layout.addWidget(self.prog_list)
        pb = QHBoxLayout()
        add_prog_btn = QPushButton("+ Program")
        set_icon(add_prog_btn, "fa5s.file-code", QStyle.SP_FileIcon)
        add_prog_btn.clicked.connect(self._add_program)
        rm_prog_btn = QPushButton("- Program")
        set_icon(rm_prog_btn, "fa5s.trash", QStyle.SP_TrashIcon)
        rm_prog_btn.clicked.connect(self._rm_program)
        pb.addWidget(add_prog_btn)
        pb.addWidget(rm_prog_btn)
        pb.addStretch()
        layout.addLayout(pb)

        # Arbitrary files
        layout.addWidget(QLabel("Files"))
        self.file_list = QTreeWidget()
        self.file_list.setHeaderLabels(["Name", "Source", "Size"])
        self.file_list.setMaximumHeight(80)
        self.file_list.setRootIsDecorated(False)
        layout.addWidget(self.file_list)
        fib = QHBoxLayout()
        add_file_btn = QPushButton("+ File")
        set_icon(add_file_btn, "fa5s.file", QStyle.SP_FileIcon)
        add_file_btn.clicked.connect(self._add_file)
        rm_file_btn = QPushButton("- File")
        set_icon(rm_file_btn, "fa5s.trash", QStyle.SP_TrashIcon)
        rm_file_btn.clicked.connect(self._rm_file)
        fib.addWidget(add_file_btn)
        fib.addWidget(rm_file_btn)
        fib.addStretch()
        layout.addLayout(fib)

        # Include dirs
        layout.addWidget(QLabel("Include Dirs (comma-separated)"))
        self.include_edit = QLineEdit()
        self.include_edit.setPlaceholderText("e.g. ../../lib")
        self.include_edit.editingFinished.connect(self._update_includes)
        layout.addWidget(self.include_edit)

        # Exec mode
        em_row = QHBoxLayout()
        em_row.addWidget(QLabel("Main exec_mode:"))
        self.exec_combo = QComboBox()
        self.exec_combo.addItems(["flash", "ram"])
        self.exec_combo.currentTextChanged.connect(lambda v: setattr(model, 'exec_mode', v))
        em_row.addWidget(self.exec_combo)
        em_row.addSpacing(20)
        em_row.addWidget(QLabel("Render mode:"))
        self.render_combo = QComboBox()
        self.render_combo.addItems(device_spec(model.device_id)["render_modes"])
        self.render_combo.currentTextChanged.connect(lambda v: v and setattr(model, 'render_mode', v))
        em_row.addWidget(self.render_combo)
        em_row.addStretch()
        layout.addLayout(em_row)

        layout.addStretch()

        model.tree_changed.connect(self.refresh)

    def refresh(self):
        self.font_list.clear()
        for f in self.model.fonts:
            size = len(base64.b64decode(f.get("combined_b64", "")))
            QTreeWidgetItem(self.font_list, [
                f.get("name", ""), str(f.get("font_id", "")),
                f"{size} B"])
        self.image_list.clear()
        for img in self.model.images:
            size = len(base64.b64decode(img.get("data_b64", "")))
            QTreeWidgetItem(self.image_list, [
                img.get("name", ""), str(img.get("image_id", "")),
                f"{size / 1024:.1f} KB"])
        self.prog_list.clear()
        for p in self.model.programs:
            QTreeWidgetItem(self.prog_list, [
                p.get("name", ""), p.get("exec_mode", "ram"),
                f"{len(base64.b64decode(p.get('source_b64', '')))} bytes"])
        self.file_list.clear()
        for fi in self.model.files:
            size = len(base64.b64decode(fi.get("data_b64", "")))
            QTreeWidgetItem(self.file_list, [
                fi.get("name", ""), fi.get("source_name", ""),
                f"{size} B"])
        self.include_edit.setText(", ".join(self.model.include_dirs))
        idx = self.exec_combo.findText(self.model.exec_mode)
        if idx >= 0:
            self.exec_combo.setCurrentIndex(idx)
        # Render-mode choices follow the project's device (may change on load).
        modes = device_spec(self.model.device_id)["render_modes"]
        if modes != [self.render_combo.itemText(i)
                     for i in range(self.render_combo.count())]:
            self.render_combo.blockSignals(True)
            self.render_combo.clear()
            self.render_combo.addItems(modes)
            self.render_combo.blockSignals(False)
        idx = self.render_combo.findText(self.model.render_mode)
        if idx >= 0:
            self.render_combo.setCurrentIndex(idx)

    def _add_font(self):
        dlg = AddFontDialog(self)
        if dlg.exec() != QDialog.Accepted:
            return

        # Lazy-import converter (requires freetype-py)
        try:
            import io as _io
            tools_dir = os.path.dirname(os.path.abspath(__file__))
            if tools_dir not in sys.path:
                sys.path.insert(0, tools_dir)
            from ferrite_font_converter import rasterize, write_resource, parse_ranges
        except ImportError as e:
            QMessageBox.critical(
                self, "Missing dependency",
                "ferrite_font_converter requires freetype-py.\n\n"
                "Install it with:\n    pip install freetype-py\n\n"
                f"Detail: {e}")
            return

        font_path = dlg.font_file()
        sizes = dlg.sizes()
        ranges_spec = dlg.ranges_spec()
        bpp = dlg.bpp()
        basename = (os.path.splitext(os.path.basename(font_path))[0]
                    .lower().replace(" ", "_"))

        try:
            codes = parse_ranges(ranges_spec)
        except ValueError as e:
            QMessageBox.critical(self, "Invalid ranges", str(e))
            return

        used_ids = {f.get("font_id", 0) for f in self.model.fonts}
        next_id = max(used_ids, default=0) + 1

        progress = QProgressDialog("Generating fonts…", "Cancel", 0, len(sizes), self)
        progress.setWindowTitle("Add Font")
        progress.setWindowModality(Qt.WindowModal)
        progress.setMinimumDuration(0)

        added = []
        errors = []

        for i, size in enumerate(sizes):
            if progress.wasCanceled():
                break
            progress.setValue(i)
            progress.setLabelText(
                f"Rendering {basename} {size} pt  ({i + 1}/{len(sizes)})…")
            QApplication.processEvents()

            while next_id in used_ids:
                next_id += 1

            name = f"{basename}_{size}"
            if any(f["name"] == name for f in self.model.fonts):
                errors.append(f"{name}: already exists, skipped")
                continue

            try:
                bitmap, table, y_advance = rasterize(font_path, size, codes, bpp)
                buf = _io.BytesIO()
                write_resource(bitmap, table, y_advance, next_id, bpp, buf)
                self.model.fonts.append({
                    "name": name,
                    "font_id": next_id,
                    "combined_b64": base64.b64encode(buf.getvalue()).decode("ascii"),
                })
                used_ids.add(next_id)
                next_id += 1
                added.append(name)
            except Exception as e:
                errors.append(f"{name}: {e}")

        progress.setValue(len(sizes))

        if added:
            self.model._res_gen += 1
            self.refresh()
            self.model.notify_changed()

        if errors:
            QMessageBox.warning(
                self, "Font generation",
                f"Added {len(added)} font(s).\n\nIssues:\n" + "\n".join(errors))

    def _rm_font(self):
        idx = self.font_list.indexOfTopLevelItem(self.font_list.currentItem())
        if idx >= 0 and idx < len(self.model.fonts):
            self.model.fonts.pop(idx)
            self.model._res_gen += 1
            self.refresh()
            self.model.notify_changed()

    def _add_image(self):
        source, _ = QFileDialog.getOpenFileName(self, "Image (PNG)", "", "Images (*.png *.jpg *.bmp)")
        if not source:
            return
        name = os.path.splitext(os.path.basename(source))[0]
        used_ids = {img.get("image_id", 0) for img in self.model.images}
        iid = 1
        while iid in used_ids:
            iid += 1
        data_b64 = base64.b64encode(open(source, "rb").read()).decode("ascii")
        self.model.images.append({
            "name": name, "image_id": iid,
            "source_name": os.path.basename(source),
            "mode": "auto", "max_colors": device_spec(self.model.device_id)["image_max_colors"],
            "data_b64": data_b64,
        })
        self.model._res_gen += 1
        self.refresh()
        self.model.notify_changed()

    def _add_image_from_svg(self):
        dlg = SvgIconPickerDialog(self)
        if dlg.exec() != QDialog.Accepted:
            return
        path = dlg.selected_path()
        if not path:
            return
        size = dlg.selected_size()
        try:
            png_bytes = SvgIconPickerDialog.render_svg_to_png(path, size)
        except Exception as e:
            QMessageBox.warning(self, "SVG Error", f"Could not render SVG:\n{e}")
            return
        name = os.path.splitext(os.path.basename(path))[0][:15]
        used_ids = {img.get("image_id", 0) for img in self.model.images}
        iid = 1
        while iid in used_ids:
            iid += 1
        self.model.images.append({
            "name": name, "image_id": iid,
            "source_name": os.path.basename(path),
            "mode": "auto", "max_colors": device_spec(self.model.device_id)["image_max_colors"],
            "data_b64": base64.b64encode(png_bytes).decode("ascii"),
        })
        self.model._res_gen += 1
        self.refresh()
        self.model.notify_changed()

    def _rm_image(self):
        idx = self.image_list.indexOfTopLevelItem(self.image_list.currentItem())
        if idx >= 0 and idx < len(self.model.images):
            self.model.images.pop(idx)
            self.model._res_gen += 1
            self.refresh()
            self.model.notify_changed()

    def _add_program(self):
        source, _ = QFileDialog.getOpenFileName(self, "Program (.fl)", "", "Ferrite (*.fl)")
        if not source:
            return
        name = os.path.splitext(os.path.basename(source))[0]
        source_b64 = base64.b64encode(open(source, "r", encoding="utf-8").read().encode("utf-8")).decode("ascii")
        self.model.programs.append({"name": name, "exec_mode": "ram", "source_b64": source_b64})
        self.refresh()

    def _rm_program(self):
        idx = self.prog_list.indexOfTopLevelItem(self.prog_list.currentItem())
        if idx >= 0 and idx < len(self.model.programs):
            self.model.programs.pop(idx)
            self.refresh()

    def _add_file(self):
        source, _ = QFileDialog.getOpenFileName(self, "Add File", "", "All Files (*)")
        if not source:
            return
        src_name = os.path.basename(source)
        # Strip extension for the resource name, max 15 chars
        name = os.path.splitext(src_name)[0][:15]
        # Ensure unique name
        existing = {fi["name"] for fi in self.model.files}
        base = name
        n = 1
        while name in existing:
            name = f"{base[:13]}_{n}"
            n += 1
        data_b64 = base64.b64encode(open(source, "rb").read()).decode("ascii")
        self.model.files.append({"name": name, "source_name": src_name, "data_b64": data_b64})
        self.refresh()
        self.model.notify_changed()

    def _rm_file(self):
        idx = self.file_list.indexOfTopLevelItem(self.file_list.currentItem())
        if 0 <= idx < len(self.model.files):
            self.model.files.pop(idx)
            self.refresh()
            self.model.notify_changed()

    def _update_includes(self):
        text = self.include_edit.text().strip()
        if text:
            self.model.include_dirs = [d.strip() for d in text.split(",") if d.strip()]
        else:
            self.model.include_dirs = []


# ============================================================
# Code generation
# ============================================================

def generate_designer_fl(model):
    """Generate main.designer.fl — layout() function with widget creation."""
    lines = []
    lines.append("// Auto-generated by Ferrite Designer - do not edit manually")
    lines.append("")

    widgets = []
    for node in model.dfs_order():
        if node is model.root:
            continue
        widgets.append(node)

    # Global variable declarations (top-level, visible to main.fl after #include)
    for w in widgets:
        lines.append(f"var {w.name};")
    if widgets:
        lines.append("")

    lines.append("fn layout() {")

    # Root bg_color
    root = model.root
    if root.bg_color != 0:
        lines.append(f"    target(0);")
        lines.append(f"    set(bg_color, 0x{root.bg_color:04X});")

    for node in widgets:
        lines.append("")
        lines.append(f"    {node.name} = alloc();")

        if node.kind != KIND_BASE:
            lines.append(f"    {node.name}.kind = {node.kind};")
        if node.loc_x != 0 or node.loc_y != 0:
            lines.append(f"    {node.name}.location = [{node.loc_x}, {node.loc_y}];")
        lines.append(f"    {node.name}.size = [{node.size_w}, {node.size_h}];")

        if node.bg_color != 0:
            lines.append(f"    {node.name}.bg_color = 0x{node.bg_color:04X};")
        if node.border_color != 0:
            lines.append(f"    {node.name}.border_color = 0x{node.border_color:04X};")
        if node.border_radius > 0:
            lines.append(f"    {node.name}.border_radius = {node.border_radius};")
        if node.text_color != 0xFFFF:
            lines.append(f"    {node.name}.text_color = 0x{node.text_color:04X};")
        if node.press_color != 0:
            lines.append(f"    {node.name}.press_color = 0x{node.press_color:04X};")
        if node.alpha != 255:
            lines.append(f"    {node.name}.alpha = {node.alpha};")

        if any(v > 0 for v in node.border):
            lines.append(f"    {node.name}.border = [{node.border[0]}, {node.border[1]}, {node.border[2]}, {node.border[3]}];")
        if any(v > 0 for v in node.margin):
            lines.append(f"    {node.name}.margin = [{node.margin[0]}, {node.margin[1]}, {node.margin[2]}, {node.margin[3]}];")
        if any(v > 0 for v in node.padding):
            lines.append(f"    {node.name}.padding = [{node.padding[0]}, {node.padding[1]}, {node.padding[2]}, {node.padding[3]}];")

        if node.text:
            escaped = node.text.replace("\\", "\\\\").replace('"', '\\"')
            lines.append(f'    {node.name}.text = "{escaped}";')
        if node.font_id != 0:
            lines.append(f"    {node.name}.font_id = {node.font_id};")
        if node.text_align != 0:
            lines.append(f"    {node.name}.text_align = {node.text_align};")

        if not node.visible:
            lines.append(f"    {node.name}.visible = 0;")
        if not node.enabled:
            lines.append(f"    {node.name}.enabled = 0;")
        if node.clickable:
            lines.append(f"    {node.name}.clickable = 1;")
        if node.value != 0:
            lines.append(f"    {node.name}.value = {node.value};")
        if node.checked:
            lines.append(f"    {node.name}.checked = 1;")
        if node.multi_line:
            lines.append(f"    {node.name}.multi_line = 1;")
        if node.image_id != 0:
            lines.append(f"    {node.name}.image_id = {node.image_id};")

        if node.on_click:
            lines.append(f"    {node.name}.on_click = @{node.on_click};")
        if node.on_tap:
            lines.append(f"    {node.name}.on_tap = @{node.on_tap};")
        if node.on_paint:
            lines.append(f"    {node.name}.on_paint = @{node.on_paint};")

        parent_ref = "0" if node.parent is model.root else node.parent.name
        lines.append(f"    parent({parent_ref});")

    lines.append("}")
    lines.append("")
    return "\n".join(lines)


# Default main.fl template — user edits this, designer never overwrites it
DEFAULT_MAIN_FL = '''\
#include "main.designer.fl"

fn setup() {
    layout();
    render();
    return 0;
}

fn loop() {
}
'''


def generate_fl(model):
    """Legacy: generate standalone .fl with setup+loop (for Export .fl)."""
    # Reuse designer output but wrap in setup/loop
    designer = generate_designer_fl(model)
    # Replace fn layout() with fn setup() and add render+return+loop
    code = designer.replace("fn layout() {", "fn setup() {")
    # Append render + return + loop before the final empty line
    code = code.rstrip()
    if code.endswith("}"):
        code = code[:-1] + "\n    render();\n    return 0;\n}\n\nfn loop() {\n}\n"
    return code


# ============================================================
# Adafruit GFX font .h → binary converter
# ============================================================

def gfx_header_to_bin(h_path, font_id=0):
    """Parse an Adafruit GFX C header (.h) and produce a sparse flash binary.

    New sparse flash binary format:
      [0..2]         num_glyphs: u16 LE
      [2]            y_advance: u8
      [3]            font_id: u8
      [4..4+N*2]     codepoints: N x u16 LE, sorted ascending (range first..last)
      [4+N*2..4+N*9] glyphs: N x 7 bytes
      [4+N*9..]      bitmap data (1-bit packed, MSB first)
    """
    import re
    import struct

    with open(h_path, "r", encoding="utf-8", errors="replace") as f:
        src = f.read()

    # Bitmap array
    bm = re.search(
        r"const\s+uint8_t\s+\w+Bitmaps\[\]\s+(?:PROGMEM\s*)?=\s*\{([^}]+)\}", src)
    if not bm:
        raise ValueError("Bitmap array not found (expected: const uint8_t ...Bitmaps[] = {...})")
    bitmap_bytes = bytes(int(x.strip(), 0) for x in bm.group(1).split(",") if x.strip())

    # Glyph array — strip C comments before parsing
    gm = re.search(
        r"const\s+GFXglyph\s+\w+Glyphs\[\]\s+(?:PROGMEM\s*)?=\s*\{(.+?)\};",
        src, re.DOTALL)
    if not gm:
        raise ValueError("Glyph array not found (expected: const GFXglyph ...Glyphs[] = {...})")

    glyph_text = re.sub(r"//[^\n]*", "", gm.group(1))
    glyphs = []
    for m in re.finditer(r"\{([^}]+)\}", glyph_text):
        vals = [v.strip() for v in m.group(1).split(",") if v.strip()]
        nums = [int(v) for v in vals[:6]]
        glyphs.append(nums)

    # Font struct (first, last, yAdvance)
    fm = re.search(
        r"const\s+GFXfont\s+\w+\s+(?:PROGMEM\s*)?=\s*\{[^,]+,[^,]+,\s*(0x[\dA-Fa-f]+|\d+)\s*,\s*(0x[\dA-Fa-f]+|\d+)\s*,\s*(\d+)\s*\}",
        src)
    if not fm:
        raise ValueError("Font struct not found (expected: const GFXfont ... = {..., first, last, yAdvance})")
    first = int(fm.group(1), 0)
    last = int(fm.group(2), 0)
    y_advance = int(fm.group(3))

    glyph_count = last - first + 1
    if len(glyphs) < glyph_count:
        raise ValueError(
            f"Expected {glyph_count} glyphs (first=0x{first:02X}, last=0x{last:02X}), found {len(glyphs)}")

    # Build new sparse binary
    out = bytearray()
    out.extend(struct.pack("<HBB", glyph_count, y_advance, font_id & 0xFF))
    for cp in range(first, last + 1):
        out.extend(struct.pack("<H", cp))
    for g in glyphs[:glyph_count]:
        offset, w, h, x_adv, x_off, y_off = g
        out.extend(struct.pack("<H", offset & 0xFFFF))
        out.append(w & 0xFF)
        out.append(h & 0xFF)
        out.append(x_adv & 0xFF)
        out.append(x_off & 0xFF)  # i8 as u8
        out.append(y_off & 0xFF)  # i8 as u8
    out.extend(bitmap_bytes)

    return bytes(out)


# ============================================================
# Code editor with syntax highlighting
# ============================================================

class FerriteSyntaxHighlighter(QSyntaxHighlighter):
    """Syntax highlighter for .fl (ferrite lang) files."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self._rules = []

        # Keywords
        kw_fmt = QTextCharFormat()
        kw_fmt.setForeground(QColor("#CC7832"))
        kw_fmt.setFontWeight(QFont.Bold)
        keywords = ["var", "fn", "if", "else", "while", "for",
                     "return", "true", "false", "break", "continue"]
        for w in keywords:
            self._rules.append((QRegularExpression(rf"\b{w}\b"), kw_fmt))

        # Built-in functions
        bi_fmt = QTextCharFormat()
        bi_fmt.setForeground(QColor("#FFC66D"))
        builtins = ["alloc", "target", "parent", "set", "get", "dirty", "render",
                     "halt", "yield_op", "delay", "millis", "critical",
                     "fillRect", "rect", "line", "circle", "fillCircle",
                     "roundedRect", "fillRoundedRect", "arc",
                     "drawImage", "drawText", "drawStr",
                     "str", "itos", "ftos", "concat", "parseInt", "parseFloat",
                     "strLen", "setText", "strClear", "strFree", "arrFree",
                     "itof", "ftoi", "fneg", "fadd", "fsub", "fmul", "fdiv",
                     "setBrightness", "brightness",
                     "rtcRead", "rtcWrite", "sendUsart",
                     "fpgaCmd", "fpgaData", "beginFrame", "endFrame",
                     "layout"]
        for w in builtins:
            self._rules.append((QRegularExpression(rf"\b{w}\b"), bi_fmt))

        # Numbers (hex, binary, decimal, float)
        num_fmt = QTextCharFormat()
        num_fmt.setForeground(QColor("#6897BB"))
        self._rules.append((QRegularExpression(r"\b0x[0-9a-fA-F_]+\b"), num_fmt))
        self._rules.append((QRegularExpression(r"\b0b[01_]+\b"), num_fmt))
        self._rules.append((QRegularExpression(r"\b\d[\d_]*\.?\d*\b"), num_fmt))

        # Strings
        str_fmt = QTextCharFormat()
        str_fmt.setForeground(QColor("#6A8759"))
        self._rules.append((QRegularExpression(r'"[^"\\]*(\\.[^"\\]*)*"'), str_fmt))

        # Function references @name
        ref_fmt = QTextCharFormat()
        ref_fmt.setForeground(QColor("#9876AA"))
        self._rules.append((QRegularExpression(r"@\w+"), ref_fmt))

        # Dot property access (widget.prop)
        dot_fmt = QTextCharFormat()
        dot_fmt.setForeground(QColor("#B0B0B0"))
        self._rules.append((QRegularExpression(r"\.\w+"), dot_fmt))

        # Preprocessor (#include)
        pre_fmt = QTextCharFormat()
        pre_fmt.setForeground(QColor("#BBB529"))
        self._rules.append((QRegularExpression(r"#include\s+\"[^\"]+\""), pre_fmt))

        # Comments
        self._comment_fmt = QTextCharFormat()
        self._comment_fmt.setForeground(QColor("#808080"))
        self._comment_fmt.setFontItalic(True)
        self._rules.append((QRegularExpression(r"//[^\n]*"), self._comment_fmt))
        self._ml_start = QRegularExpression(r"/\*")
        self._ml_end = QRegularExpression(r"\*/")

    def highlightBlock(self, text):
        # Single-line rules
        for pattern, fmt in self._rules:
            it = pattern.globalMatch(text)
            while it.hasNext():
                match = it.next()
                self.setFormat(match.capturedStart(), match.capturedLength(), fmt)

        # Multi-line comments
        self.setCurrentBlockState(0)
        start_idx = 0
        if self.previousBlockState() != 1:
            match = self._ml_start.match(text)
            start_idx = match.capturedStart() if match.hasMatch() else -1
        while start_idx >= 0:
            end_match = self._ml_end.match(text, start_idx + 2)
            if end_match.hasMatch():
                length = end_match.capturedEnd() - start_idx
            else:
                self.setCurrentBlockState(1)
                length = len(text) - start_idx
            self.setFormat(start_idx, length, self._comment_fmt)
            match = self._ml_start.match(text, start_idx + length)
            start_idx = match.capturedStart() if match.hasMatch() else -1


class LineNumberArea(QWidget):
    """Line number gutter for the code editor."""

    def __init__(self, editor):
        super().__init__(editor)
        self._editor = editor

    def sizeHint(self):
        return QSize(self._editor.line_number_area_width(), 0)

    def paintEvent(self, event):
        self._editor.line_number_area_paint(event)


class CodeEditorWidget(QPlainTextEdit):
    """Plain text editor with line numbers and ferrite syntax highlighting."""

    def __init__(self, parent=None):
        super().__init__(parent)
        font = QFontDatabase.systemFont(QFontDatabase.FixedFont)
        font.setPointSize(10)
        self.setFont(font)
        self.setTabStopDistance(self.fontMetrics().horizontalAdvance(' ') * 4)
        self.setLineWrapMode(QPlainTextEdit.NoWrap)

        self._highlighter = FerriteSyntaxHighlighter(self.document())
        self._line_area = LineNumberArea(self)

        self.blockCountChanged.connect(self._update_line_area_width)
        self.updateRequest.connect(self._update_line_area)
        self._update_line_area_width()

        # Dark background
        self.setStyleSheet(
            "QPlainTextEdit { background-color: #2B2B2B; color: #A9B7C6; "
            "selection-background-color: #214283; }")

    def line_number_area_width(self):
        digits = max(1, len(str(self.blockCount())))
        return 16 + self.fontMetrics().horizontalAdvance('9') * digits

    def _update_line_area_width(self):
        self.setViewportMargins(self.line_number_area_width(), 0, 0, 0)

    def _update_line_area(self, rect, dy):
        if dy:
            self._line_area.scroll(0, dy)
        else:
            self._line_area.update(0, rect.y(), self._line_area.width(), rect.height())
        if rect.contains(self.viewport().rect()):
            self._update_line_area_width()

    def resizeEvent(self, event):
        super().resizeEvent(event)
        cr = self.contentsRect()
        self._line_area.setGeometry(cr.left(), cr.top(),
                                     self.line_number_area_width(), cr.height())

    def line_number_area_paint(self, event):
        painter = QPainter(self._line_area)
        painter.fillRect(event.rect(), QColor("#313335"))

        # Separator line
        w = self._line_area.width()
        painter.setPen(QColor("#444444"))
        painter.drawLine(w - 1, event.rect().top(), w - 1, event.rect().bottom())

        painter.setPen(QColor("#606366"))
        painter.setFont(self.font())

        block = self.firstVisibleBlock()
        block_num = block.blockNumber()
        top = int(self.blockBoundingGeometry(block).translated(self.contentOffset()).top())
        bottom = top + int(self.blockBoundingRect(block).height())

        while block.isValid() and top <= event.rect().bottom():
            if block.isVisible() and bottom >= event.rect().top():
                painter.drawText(4, top, w - 10,
                                 self.fontMetrics().height(),
                                 Qt.AlignRight, str(block_num + 1))
            block = block.next()
            top = bottom
            bottom = top + int(self.blockBoundingRect(block).height())
            block_num += 1


class CodeEditorPanel(QWidget):
    """Panel containing the code editor for main.fl — reads/writes from model."""

    def __init__(self, model):
        super().__init__()
        self.model = model
        self._modified = False

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        # Toolbar
        tb = QHBoxLayout()
        tb.setContentsMargins(4, 4, 4, 4)
        self._file_label = QLabel("main.fl")
        self._file_label.setStyleSheet("font-weight: bold;")
        tb.addWidget(self._file_label)
        tb.addStretch()
        save_btn = QPushButton("Save to Project")
        set_icon(save_btn, "fa5s.save", QStyle.SP_DialogSaveButton)
        save_btn.clicked.connect(self.save_file)
        tb.addWidget(save_btn)
        reload_btn = QPushButton("Revert")
        set_icon(reload_btn, "fa5s.undo", QStyle.SP_BrowserReload)
        reload_btn.clicked.connect(self.load_file)
        tb.addWidget(reload_btn)
        layout.addLayout(tb)

        # Editor
        self.editor = CodeEditorWidget()
        self.editor.textChanged.connect(self._on_text_changed)
        layout.addWidget(self.editor)

    def _on_text_changed(self):
        self._modified = True
        self._file_label.setText("main.fl *")

    def load_file(self):
        """Load main.fl content from model."""
        self.editor.blockSignals(True)
        self.editor.setPlainText(self.model.main_fl)
        self.editor.blockSignals(False)
        self._modified = False
        self._file_label.setText("main.fl")

    def save_file(self):
        """Save editor content back to model."""
        self.model.main_fl = self.editor.toPlainText()
        self._modified = False
        self._file_label.setText("main.fl")

    def is_modified(self):
        return self._modified

    def ensure_saved(self):
        """Save if modified. Returns True if OK to proceed, False if cancelled."""
        if not self._modified:
            return True
        ret = QMessageBox.question(
            self, "Unsaved Changes",
            "main.fl has unsaved changes. Save before continuing?",
            QMessageBox.Save | QMessageBox.Discard | QMessageBox.Cancel)
        if ret == QMessageBox.Save:
            self.save_file()
            return True
        elif ret == QMessageBox.Discard:
            return True
        return False


# ============================================================
# Code preview dialog
# ============================================================

class CodeDialog(QDialog):
    def __init__(self, code, parent=None):
        super().__init__(parent)
        self.setWindowTitle("Generated Code")
        self.resize(700, 600)
        layout = QVBoxLayout(self)
        self.editor = QTextEdit()
        self.editor.setPlainText(code)
        self.editor.setReadOnly(True)
        self.editor.setFont(QFont("Courier", 10))
        layout.addWidget(self.editor)
        btns = QDialogButtonBox()
        copy_btn = btns.addButton("Copy", QDialogButtonBox.ActionRole)
        set_icon(copy_btn, "fa5s.copy", QStyle.SP_FileIcon)
        save_btn = btns.addButton("Save .fl", QDialogButtonBox.ActionRole)
        set_icon(save_btn, "fa5s.save", QStyle.SP_DialogSaveButton)
        btns.addButton(QDialogButtonBox.Close)
        copy_btn.clicked.connect(lambda: QApplication.clipboard().setText(code))
        save_btn.clicked.connect(lambda: self._save(code))
        btns.rejected.connect(self.reject)
        layout.addWidget(btns)

    def _save(self, code):
        path, _ = QFileDialog.getSaveFileName(self, "Save .fl", "", "Ferrite (*.fl)")
        if path:
            with open(path, "w", encoding="utf-8") as f:
                f.write(code)


# ============================================================
# Project device selection
# ============================================================

class DeviceChoiceDialog(QDialog):
    """Choose the target device for a new designer project."""

    def __init__(self, parent=None, device_id=DEFAULT_DEVICE_ID):
        super().__init__(parent)
        self.setWindowTitle("Choose Device")
        self.setModal(True)

        layout = QVBoxLayout(self)
        layout.addWidget(QLabel("Select the target device for this project:"))

        self.combo = QComboBox()
        for key, spec in load_devices().items():
            w, h = spec["screen"]
            self.combo.addItem(f"{spec['name']} ({w}x{h}, {spec['color_model']})", key)
        idx = self.combo.findData(device_id)
        self.combo.setCurrentIndex(max(0, idx))
        self.combo.currentIndexChanged.connect(self._update_details)
        layout.addWidget(self.combo)

        self.details = QLabel()
        self.details.setWordWrap(True)
        layout.addWidget(self.details)

        buttons = QDialogButtonBox(QDialogButtonBox.Ok | QDialogButtonBox.Cancel)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        layout.addWidget(buttons)

        self._update_details()

    def selected_device(self):
        return self.combo.currentData() or DEFAULT_DEVICE_ID

    def _update_details(self):
        spec = device_spec(self.selected_device())
        w, h = spec["screen"]
        self.details.setText(
            f"Screen: {w}x{h} ({spec.get('shape', 'rect')})\n"
            f"Colors: {spec['color_model']} ({spec['color_count']} supported)\n"
            f"Render modes: {', '.join(spec['render_modes'])} "
            f"(default: {spec['render_mode']})"
        )


# ============================================================
# Device communication
# ============================================================

try:
    import serial
    import serial.tools.list_ports
    HAS_SERIAL = True
except ImportError:
    HAS_SERIAL = False


class DeviceDialog(QDialog):
    """Dialog for serial device communication — ping, upload, writefs."""

    def __init__(self, model, parent=None):
        super().__init__(parent)
        self.model = model
        self._parent_window = parent
        self.setWindowTitle("Device")
        self.resize(550, 480)
        self._device_alive = False

        layout = QVBoxLayout(self)

        # Port selection + status
        port_row = QHBoxLayout()
        port_row.addWidget(QLabel("Port:"))
        self.port_combo = QComboBox()
        self.port_combo.setEditable(True)
        self.port_combo.setMinimumWidth(200)
        self.port_combo.currentIndexChanged.connect(self._auto_ping)
        port_row.addWidget(self.port_combo)
        refresh_btn = QPushButton("Refresh")
        set_icon(refresh_btn, "fa5s.sync", QStyle.SP_BrowserReload)
        refresh_btn.clicked.connect(self._refresh_ports)
        port_row.addWidget(refresh_btn)
        self.status_label = QLabel("")
        self.status_label.setFixedWidth(20)
        port_row.addWidget(self.status_label)
        port_row.addStretch()
        layout.addLayout(port_row)

        # Action buttons
        btn_row = QHBoxLayout()
        ping_btn = QPushButton("Ping")
        set_icon(ping_btn, "fa5s.satellite-dish", QStyle.SP_DialogYesButton)
        ping_btn.clicked.connect(self._ping)
        btn_row.addWidget(ping_btn)
        restart_btn = QPushButton("Restart")
        set_icon(restart_btn, "fa5s.power-off", QStyle.SP_BrowserReload)
        restart_btn.clicked.connect(self._restart)
        btn_row.addWidget(restart_btn)
        meminfo_btn = QPushButton("MemInfo")
        set_icon(meminfo_btn, "fa5s.memory", QStyle.SP_FileDialogInfoView)
        meminfo_btn.clicked.connect(self._meminfo)
        btn_row.addWidget(meminfo_btn)
        btn_row.addStretch()
        layout.addLayout(btn_row)

        # Upload section
        layout.addWidget(QLabel(""))
        upload_group = QGroupBox("Upload to Device")
        ug_layout = QVBoxLayout(upload_group)

        info_label = QLabel(
            "Device accepts uploads in any state (normal, error, or recovery).\n"
            "No need to enter recovery mode — just click Upload.")
        info_label.setStyleSheet("color: #888; font-size: 11px;")
        info_label.setWordWrap(True)
        ug_layout.addWidget(info_label)

        build_upload_btn = QPushButton("Build && Upload Flash Image")
        set_icon(build_upload_btn, "fa5s.cloud-upload-alt", QStyle.SP_ArrowUp)
        build_upload_btn.setStyleSheet("font-weight: bold; padding: 8px;")
        build_upload_btn.clicked.connect(self._build_and_writefs)
        ug_layout.addWidget(build_upload_btn)

        file_row = QHBoxLayout()
        writefs_btn = QPushButton("Upload flash.bin...")
        set_icon(writefs_btn, "fa5s.upload", QStyle.SP_ArrowUp)
        writefs_btn.clicked.connect(self._writefs_file)
        file_row.addWidget(writefs_btn)
        exec_btn = QPushButton("Execute .bin...")
        set_icon(exec_btn, "fa5s.play", QStyle.SP_MediaPlay)
        exec_btn.clicked.connect(self._execute_file)
        file_row.addWidget(exec_btn)
        ug_layout.addLayout(file_row)
        layout.addWidget(upload_group)

        # Log output
        self.log = QTextEdit()
        self.log.setReadOnly(True)
        self.log.setFont(QFont("Courier", 9))
        self.log.setMinimumHeight(150)
        layout.addWidget(self.log)

        # Close button
        close_btn = QPushButton("Close")
        set_icon(close_btn, "fa5s.times", QStyle.SP_DialogCloseButton)
        close_btn.clicked.connect(self.accept)
        layout.addWidget(close_btn)

        self._refresh_ports()
        # Auto-ping after ports are listed
        if self.port_combo.count() > 0:
            from PySide6.QtCore import QTimer
            QTimer.singleShot(300, self._auto_ping)

    def _set_status(self, alive):
        self._device_alive = alive
        if alive:
            self.status_label.setStyleSheet(
                "background-color: #00CC00; border-radius: 10px; min-width: 20px; min-height: 20px;")
            self.status_label.setToolTip("Device connected")
        else:
            self.status_label.setStyleSheet(
                "background-color: #CC0000; border-radius: 10px; min-width: 20px; min-height: 20px;")
            self.status_label.setToolTip("Device not responding")

    def _auto_ping(self):
        """Silent ping to check device status."""
        port = self._get_port()
        if not port or not HAS_SERIAL:
            self._set_status(False)
            return
        try:
            ser = serial.Serial(port=port, baudrate=115200,
                                bytesize=serial.EIGHTBITS, parity=serial.PARITY_NONE,
                                stopbits=serial.STOPBITS_ONE, timeout=1.0)
            self._send_varint_msg(ser, 0x08)
            resp, val = self._read_response(ser)
            ser.close()
            alive = resp == "pong"
            self._set_status(alive)
            if alive:
                self._log(f"Device on {port} — connected")
            else:
                self._log(f"Device on {port} — no response")
        except Exception:
            self._set_status(False)

    def _log(self, msg):
        self.log.append(msg)
        self.log.verticalScrollBar().setValue(self.log.verticalScrollBar().maximum())
        QApplication.processEvents()

    def _get_port(self):
        return self.port_combo.currentText().strip()

    def _refresh_ports(self):
        self.port_combo.clear()
        if not HAS_SERIAL:
            self._log("pyserial not installed: pip install pyserial")
            return
        ports = serial.tools.list_ports.comports()
        for p in sorted(ports, key=lambda x: x.device):
            desc = f"{p.device} — {p.description}" if p.description != p.device else p.device
            self.port_combo.addItem(p.device)
            self.port_combo.setItemData(self.port_combo.count() - 1, desc, Qt.ToolTipRole)
        if ports:
            self._log(f"Found {len(ports)} port(s)")
        else:
            self._log("No serial ports found")

    def _open_serial(self, timeout=5.0):
        port = self._get_port()
        if not port:
            self._log("ERROR: No port selected")
            return None
        if not HAS_SERIAL:
            self._log("ERROR: pyserial not installed: pip install pyserial")
            return None
        try:
            ser = serial.Serial(port=port, baudrate=115200,
                                bytesize=serial.EIGHTBITS, parity=serial.PARITY_NONE,
                                stopbits=serial.STOPBITS_ONE, timeout=timeout)
            return ser
        except Exception as e:
            self._log(f"ERROR: {e}")
            return None

    def _send_varint_msg(self, ser, tag):
        ser.write(bytes([tag]))
        ser.flush()

    def _send_payload_msg(self, ser, tag, payload):
        out = bytearray([tag])
        length = len(payload)
        while length > 0x7F:
            out.append((length & 0x7F) | 0x80)
            length >>= 7
        out.append(length & 0x7F)
        out.extend(payload)
        ser.write(bytes(out))
        ser.flush()

    def _read_response(self, ser):
        """Read one protocol response. Returns (type_str, value)."""
        import struct as st
        import time
        deadline = time.time() + ser.timeout

        while time.time() < deadline:
            remaining = max(0.1, deadline - time.time())
            ser.timeout = remaining
            b = ser.read(1)
            if not b:
                return ("timeout", None)
            tag = b[0]

            if tag == 0x10:  # PONG
                return ("pong", None)
            elif tag == 0x0A:  # ERROR
                lb = ser.read(1)
                if lb:
                    length = lb[0]
                    data = ser.read(length)
                    code = data[0] if data else 0
                    return ("error", code)
            elif tag == 0x18:  # MEMINFO_RESP
                val = 0
                shift = 0
                while True:
                    vb = ser.read(1)
                    if not vb:
                        break
                    byte = vb[0]
                    val |= (byte & 0x7F) << shift
                    shift += 7
                    if not (byte & 0x80):
                        break
                return ("meminfo", val)
            # Skip unknown bytes (debug output)
        return ("timeout", None)

    def _ping(self):
        ser = self._open_serial()
        if not ser:
            self._set_status(False)
            return
        self._log(f"Ping {ser.port}...")
        self._send_varint_msg(ser, 0x08)
        resp, val = self._read_response(ser)
        if resp == "pong":
            self._log("PONG — device is alive")
            self._set_status(True)
        elif resp == "error":
            self._log(f"ERROR: code {val}")
            self._set_status(False)
        else:
            self._log("TIMEOUT — no response")
            self._set_status(False)
        ser.close()

    def _restart(self):
        ser = self._open_serial()
        if not ser:
            return
        self._send_varint_msg(ser, 0x18)
        self._log("Restart sent")
        ser.close()

    def _meminfo(self):
        ser = self._open_serial()
        if not ser:
            return
        self._log("Querying memory...")
        self._send_varint_msg(ser, 0x38)
        resp, val = self._read_response(ser)
        if resp == "meminfo":
            self._log(f"Free heap: {val} bytes ({val / 1024:.1f} KB)")
        elif resp == "error":
            self._log(f"ERROR: code {val}")
        else:
            self._log("TIMEOUT")
        ser.close()

    def _writefs(self, data, label="flash.bin"):
        """Upload flash filesystem image with chunked protocol."""
        import struct as st
        CHUNK_SIZE = 4096

        ser = self._open_serial(timeout=10.0)
        if not ser:
            return False

        total = len(data)
        chunk_count = (total + CHUNK_SIZE - 1) // CHUNK_SIZE
        self._log(f"Uploading {label}: {total} bytes ({total / 1024:.1f} KB), {chunk_count} chunks")

        # Phase 1: Send header (total_size + chunk_size)
        header = st.pack("<II", total, CHUNK_SIZE)
        self._send_payload_msg(ser, 0x22, header)

        # Wait for ACK
        resp, val = self._read_response(ser)
        if resp != "pong":
            self._log(f"ERROR: Expected ACK, got {resp} (code {val})")
            ser.close()
            return False
        self._log("Device ACK — sending chunks...")

        # Phase 2: Send chunks
        for i in range(chunk_count):
            offset = i * CHUNK_SIZE
            chunk = data[offset:offset + CHUNK_SIZE]
            self._send_payload_msg(ser, 0x2A, chunk)

            resp, val = self._read_response(ser)
            if resp != "pong":
                self._log(f"\nERROR: Chunk {i + 1} failed: {resp} (code {val})")
                ser.close()
                return False

            pct = (i + 1) * 100 // chunk_count
            # Update last log line
            self.log.moveCursor(QTextCursor.MoveOperation.End)
            self._log(f"  [{i + 1}/{chunk_count}] {pct}%")

        self._log(f"Upload complete! Device will restart.")
        ser.close()
        self._set_status(False)  # device is restarting
        # Wait a moment, then re-ping to confirm boot
        from PySide6.QtCore import QTimer
        QTimer.singleShot(2000, self._auto_ping)
        return True

    def _build_and_writefs(self):
        """Build flash image and upload to device."""
        if not self.model._path:
            self._log("ERROR: Save the project first (Ctrl+S)")
            return

        self._log("Building flash image...")
        QApplication.processEvents()

        try:
            proj_dir = self.model.project_dir() or tempfile.mkdtemp(prefix="ferrite_")
            build_dir = os.path.join(proj_dir, ".build")
            tools_dir = os.path.dirname(os.path.abspath(__file__))
            json_path = self.model.extract_to_dir(build_dir)
            output_path = os.path.join(build_dir, "flash.bin")
            build_script = os.path.join(tools_dir, "ferrite_build.py")
            result = subprocess.run(
                [sys.executable, build_script, json_path, "-o", output_path],
                capture_output=True, text=True, cwd=tools_dir, timeout=30)
            if result.returncode != 0:
                self._log(f"BUILD FAILED:\n{result.stderr or result.stdout}")
                return
        except Exception as e:
            self._log(f"BUILD ERROR: {e}")
            return

        size = os.path.getsize(output_path)
        self._log(f"Build OK: {size} bytes ({size / 1024:.1f} KB)")

        # Upload
        with open(output_path, "rb") as f:
            data = f.read()
        self._writefs(data, "flash.bin")

    def _writefs_file(self):
        path, _ = QFileDialog.getOpenFileName(self, "Flash Image", "", "Binary (*.bin)")
        if not path:
            return
        with open(path, "rb") as f:
            data = f.read()
        self._writefs(data, os.path.basename(path))

    def _execute_file(self):
        path, _ = QFileDialog.getOpenFileName(self, "Program Binary", "", "Binary (*.bin *.fxe)")
        if not path:
            return
        with open(path, "rb") as f:
            payload = f.read()
        if len(payload) > 2048:
            self._log(f"ERROR: Program too large ({len(payload)} bytes, max 2048). Use writefs instead.")
            return
        ser = self._open_serial()
        if not ser:
            return
        self._send_payload_msg(ser, 0x12, payload)
        self._log(f"Executed: {len(payload)} bytes sent")
        ser.close()


# ============================================================
# Main window
# ============================================================

class DesignerWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("Ferrite UI Designer")
        self.resize(1280, 760)

        self.model = DesignerModel()
        self.scene = DesignerScene(self.model)
        self.canvas = CanvasView(self.scene)
        self.tree = TreePanel(self.model)
        self.props = PropertyEditor(self.model)
        self.resources = ResourcePanel(self.model)
        self.code_editor = CodeEditorPanel(self.model)

        self.center_tabs = QTabWidget()
        self.center_tabs.addTab(self.canvas, "Design")
        self.center_tabs.addTab(self.code_editor, "Code (main.fl)")
        self.center_tabs.currentChanged.connect(self._on_tab_changed)
        self.setCentralWidget(self.center_tabs)

        self.setDockOptions(
            QMainWindow.DockOption.AnimatedDocks |
            QMainWindow.DockOption.AllowNestedDocks |
            QMainWindow.DockOption.AllowTabbedDocks
        )

        self._dock_tree = QDockWidget("Widget Tree", self)
        self._dock_tree.setObjectName("dock_tree")
        self._dock_tree.setWidget(self.tree)
        self.addDockWidget(Qt.LeftDockWidgetArea, self._dock_tree)

        self._dock_props = QDockWidget("Properties", self)
        self._dock_props.setObjectName("dock_props")
        self._dock_props.setWidget(self.props)
        self.addDockWidget(Qt.LeftDockWidgetArea, self._dock_props)
        self.splitDockWidget(self._dock_tree, self._dock_props, Qt.Vertical)

        self._dock_resources = QDockWidget("Resources", self)
        self._dock_resources.setObjectName("dock_resources")
        self._dock_resources.setWidget(self.resources)
        self.addDockWidget(Qt.RightDockWidgetArea, self._dock_resources)

        self.resizeDocks(
            [self._dock_tree, self._dock_props], [220, 380], Qt.Vertical
        )
        self.resizeDocks([self._dock_tree], [260], Qt.Horizontal)
        self.resizeDocks([self._dock_resources], [260], Qt.Horizontal)

        self._build_toolbar()
        self._build_menu()
        self.statusBar().showMessage(self._device_status())

        s = self._settings()
        if s.value("geometry"):
            self.restoreGeometry(s.value("geometry"))
        if s.value("windowState"):
            self.restoreState(s.value("windowState"))

    def _device_status(self):
        spec = device_spec(self.model.device_id)
        return f"{spec['name']} - {self.model.screen_w}x{self.model.screen_h}, {spec['color_model']}"

    def _set_window_title(self, path=None):
        spec = device_spec(self.model.device_id)
        suffix = f" - {spec['name']}"
        if path:
            self.setWindowTitle(f"Ferrite UI Designer - {os.path.basename(path)}{suffix}")
        else:
            self.setWindowTitle(f"Ferrite UI Designer{suffix}")

    def _apply_new_device(self, device_id):
        self.model.clear(device_id)
        self.model._path = None
        self.resources.refresh()
        self.code_editor.load_file()
        self.center_tabs.setCurrentIndex(0)
        self.canvas.fitInView(self.scene.sceneRect(), Qt.KeepAspectRatio)
        self._set_window_title()
        self.statusBar().showMessage(self._device_status(), 5000)

    def _build_toolbar(self):
        tb = self.addToolBar("Widgets")
        tb.setObjectName("tb_widgets")
        tb.setMovable(False)
        for idx, kind in enumerate(KIND_VALUES):
            act = QAction(f"+ {KIND_NAMES[idx]}", self)
            set_icon(act, KIND_ICON_NAMES.get(kind), QStyle.SP_FileIcon)
            act.triggered.connect(lambda checked, k=kind: self.model.add_widget(k))
            tb.addAction(act)
        tb.addSeparator()
        del_act = QAction("Delete", self)
        set_icon(del_act, "fa5s.trash", QStyle.SP_TrashIcon)
        del_act.setToolTip("Delete selected widget (Del)")
        del_act.triggered.connect(self._delete_selected)
        tb.addAction(del_act)
        front_act = QAction("Bring to Front", self)
        set_icon(front_act, "fa5s.arrow-up", QStyle.SP_ArrowUp)
        front_act.setToolTip("Bring selected widget to front (])")
        front_act.triggered.connect(self._bring_to_front)
        tb.addAction(front_act)
        back_act = QAction("Send to Back", self)
        set_icon(back_act, "fa5s.arrow-down", QStyle.SP_ArrowDown)
        back_act.setToolTip("Send selected widget to back ([)")
        back_act.triggered.connect(self._send_to_back)
        tb.addAction(back_act)
        tb.addSeparator()
        snap_act = QAction("Snap Grid", self)
        set_icon(snap_act, "fa5s.th", QStyle.SP_FileDialogDetailedView)
        snap_act.setCheckable(True)
        snap_act.toggled.connect(lambda v: setattr(self.model, 'snap_to_grid', v))
        tb.addAction(snap_act)
        tb.addSeparator()
        code_act = QAction("Generate Code", self)
        set_icon(code_act, "fa5s.code", QStyle.SP_FileIcon)
        code_act.setShortcut(QKeySequence("Ctrl+G"))
        code_act.triggered.connect(self._show_code)
        tb.addAction(code_act)
        tb.addSeparator()
        build_act = QAction("Build Flash", self)
        set_icon(build_act, "fa5s.hammer", QStyle.SP_DriveHDIcon)
        build_act.setShortcut(QKeySequence("Ctrl+B"))
        build_act.triggered.connect(self._build_flash)
        tb.addAction(build_act)
        upload_act = QAction("Upload", self)
        set_icon(upload_act, "fa5s.upload", QStyle.SP_ArrowUp)
        upload_act.setShortcut(QKeySequence("Ctrl+U"))
        upload_act.triggered.connect(self._build_and_upload)
        tb.addAction(upload_act)

        # Program-wide serial port selector
        tb.addSeparator()
        tb.addWidget(QLabel(" Port: "))
        self.port_combo = QComboBox()
        self.port_combo.setObjectName("port_combo_main")
        self.port_combo.setEditable(True)
        self.port_combo.setMinimumWidth(100)
        tb.addWidget(self.port_combo)
        refresh_act = QAction("Scan", self)
        set_icon(refresh_act, "fa5s.sync", QStyle.SP_BrowserReload)
        refresh_act.triggered.connect(self._refresh_ports)
        tb.addAction(refresh_act)
        self._refresh_ports()

        # Second toolbar: clipboard + align + size
        tb2 = self.addToolBar("Align")
        tb2.setObjectName("tb_align")
        tb2.setMovable(False)

        copy_act = QAction("Copy", self)
        set_icon(copy_act, "fa5s.copy", QStyle.SP_FileIcon)
        copy_act.setToolTip("Copy selected widget (Ctrl+C)")
        copy_act.setShortcut(QKeySequence("Ctrl+C"))
        copy_act.triggered.connect(self._copy_widget)
        tb2.addAction(copy_act)

        paste_act = QAction("Paste", self)
        set_icon(paste_act, "fa5s.paste", QStyle.SP_FileIcon)
        paste_act.setToolTip("Paste widget (Ctrl+V)")
        paste_act.setShortcut(QKeySequence("Ctrl+V"))
        paste_act.triggered.connect(self._paste_widget)
        tb2.addAction(paste_act)

        dup_act = QAction("Duplicate", self)
        set_icon(dup_act, "fa5s.clone", QStyle.SP_FileIcon)
        dup_act.setToolTip("Duplicate selected widget (Ctrl+D)")
        dup_act.setShortcut(QKeySequence("Ctrl+D"))
        dup_act.triggered.connect(self._duplicate_widget)
        tb2.addAction(dup_act)

        tb2.addSeparator()
        tb2.addWidget(QLabel(" Align: "))

        for label, tip, slot, icon in (
            ("⇐L", "Align left edges",        self._align_left,     "fa5s.align-left"),
            ("⇒R", "Align right edges",        self._align_right,    "fa5s.align-right"),
            ("⇑T", "Align top edges",          self._align_top,      "fa5s.align-top"),
            ("⇓B", "Align bottom edges",       self._align_bottom,   "fa5s.align-bottom"),
            ("⇔H", "Center horizontally",      self._center_h,       "fa5s.center-h"),
            ("⇕V", "Center vertically",        self._center_v,       "fa5s.center-v"),
        ):
            act = QAction(label, self)
            act.setToolTip(tip)
            act.triggered.connect(slot)
            set_icon(act, icon)
            tb2.addAction(act)

        tb2.addSeparator()
        tb2.addWidget(QLabel(" Size: "))

        sw_act = QAction("=W", self)
        sw_act.setToolTip("Make same width as primary selection")
        sw_act.triggered.connect(self._same_width)
        tb2.addAction(sw_act)

        sh_act = QAction("=H", self)
        sh_act.setToolTip("Make same height as primary selection")
        sh_act.triggered.connect(self._same_height)
        tb2.addAction(sh_act)

    def _build_menu(self):
        mb = self.menuBar()

        file_menu = mb.addMenu("&File")
        new_act = file_menu.addAction("&New")
        set_icon(new_act, "fa5s.file", QStyle.SP_FileIcon)
        new_act.setShortcut(QKeySequence.New)
        new_act.triggered.connect(self._new_project)
        open_act = file_menu.addAction("&Open...")
        set_icon(open_act, "fa5s.folder-open", QStyle.SP_DialogOpenButton)
        open_act.setShortcut(QKeySequence.Open)
        open_act.triggered.connect(self._open_project)
        save_act = file_menu.addAction("&Save")
        set_icon(save_act, "fa5s.save", QStyle.SP_DialogSaveButton)
        save_act.setShortcut(QKeySequence.Save)
        save_act.triggered.connect(self._save_project)
        saveas_act = file_menu.addAction("Save &As...")
        set_icon(saveas_act, "fa5s.save", QStyle.SP_DialogSaveButton)
        saveas_act.setShortcut(QKeySequence("Ctrl+Shift+S"))
        saveas_act.triggered.connect(self._save_project_as)
        file_menu.addSeparator()
        self.recent_menu = file_menu.addMenu("&Recent Projects")
        self._rebuild_recent_menu()
        file_menu.addSeparator()
        export_act = file_menu.addAction("&Export .fl...")
        set_icon(export_act, "fa5s.file-export", QStyle.SP_FileIcon)
        export_act.setShortcut(QKeySequence("Ctrl+E"))
        export_act.triggered.connect(self._export_fl)
        file_menu.addSeparator()
        build_act2 = file_menu.addAction("&Build Flash Image")
        set_icon(build_act2, "fa5s.hammer", QStyle.SP_DriveHDIcon)
        build_act2.setShortcut(QKeySequence("Ctrl+B"))
        build_act2.triggered.connect(self._build_flash)
        file_menu.addSeparator()
        quit_act = file_menu.addAction("&Quit")
        set_icon(quit_act, "fa5s.times", QStyle.SP_DialogCloseButton)
        quit_act.setShortcut(QKeySequence.Quit)
        quit_act.triggered.connect(self.close)

        edit_menu = mb.addMenu("&Edit")
        del_act = edit_menu.addAction("&Delete Widget")
        set_icon(del_act, "fa5s.trash", QStyle.SP_TrashIcon)
        del_act.setShortcut(QKeySequence.Delete)
        del_act.triggered.connect(self._delete_selected)
        edit_menu.addSeparator()
        copy_act2 = edit_menu.addAction("&Copy")
        set_icon(copy_act2, "fa5s.copy", QStyle.SP_FileIcon)
        copy_act2.setShortcut(QKeySequence("Ctrl+C"))
        copy_act2.triggered.connect(self._copy_widget)
        paste_act2 = edit_menu.addAction("&Paste")
        set_icon(paste_act2, "fa5s.paste", QStyle.SP_FileIcon)
        paste_act2.setShortcut(QKeySequence("Ctrl+V"))
        paste_act2.triggered.connect(self._paste_widget)
        dup_act2 = edit_menu.addAction("D&uplicate")
        set_icon(dup_act2, "fa5s.clone", QStyle.SP_FileIcon)
        dup_act2.setShortcut(QKeySequence("Ctrl+D"))
        dup_act2.triggered.connect(self._duplicate_widget)
        edit_menu.addSeparator()
        front_act2 = edit_menu.addAction("Bring to &Front")
        set_icon(front_act2, "fa5s.arrow-up", QStyle.SP_ArrowUp)
        front_act2.setShortcut(QKeySequence("]"))
        front_act2.triggered.connect(self._bring_to_front)
        back_act2 = edit_menu.addAction("Send to &Back")
        set_icon(back_act2, "fa5s.arrow-down", QStyle.SP_ArrowDown)
        back_act2.setShortcut(QKeySequence("["))
        back_act2.triggered.connect(self._send_to_back)
        edit_menu.addSeparator()
        align_menu = edit_menu.addMenu("&Align")
        for label, slot in (
            ("Align &Left",             self._align_left),
            ("Align &Right",            self._align_right),
            ("Align &Top",              self._align_top),
            ("Align &Bottom",           self._align_bottom),
            ("Center &Horizontally",    self._center_h),
            ("Center &Vertically",      self._center_v),
        ):
            align_menu.addAction(label).triggered.connect(slot)
        size_menu = edit_menu.addMenu("&Size")
        size_menu.addAction("Same &Width").triggered.connect(self._same_width)
        size_menu.addAction("Same &Height").triggered.connect(self._same_height)

        device_menu = mb.addMenu("&Device")
        device_act = device_menu.addAction("&Device Manager...")
        set_icon(device_act, "fa5s.microchip", QStyle.SP_ComputerIcon)
        device_act.triggered.connect(self._show_device)
        device_menu.addSeparator()
        build_upload_act = device_menu.addAction("Build && &Upload")
        set_icon(build_upload_act, "fa5s.cloud-upload-alt", QStyle.SP_ArrowUp)
        build_upload_act.setShortcut(QKeySequence("Ctrl+U"))
        build_upload_act.triggered.connect(self._build_and_upload)

        view_menu = mb.addMenu("&View")
        fit_act = view_menu.addAction("&Fit to Window")
        set_icon(fit_act, "fa5s.expand-arrows-alt", QStyle.SP_DesktopIcon)
        fit_act.setShortcut(QKeySequence("Ctrl+0"))
        fit_act.triggered.connect(lambda: self.canvas.fitInView(
            self.scene.sceneRect(), Qt.KeepAspectRatio))
        view_menu.addSeparator()
        view_menu.addAction(self._dock_tree.toggleViewAction())
        view_menu.addAction(self._dock_props.toggleViewAction())
        view_menu.addAction(self._dock_resources.toggleViewAction())

    def closeEvent(self, event):
        s = self._settings()
        s.setValue("geometry", self.saveGeometry())
        s.setValue("windowState", self.saveState())
        super().closeEvent(event)

    def _on_tab_changed(self, index):
        if index == 1:  # Code tab — show main.fl from model
            if not self.code_editor.is_modified():
                self.code_editor.load_file()

    def _refresh_ports(self):
        self.port_combo.clear()
        if HAS_SERIAL:
            for p in sorted(serial.tools.list_ports.comports(), key=lambda x: x.device):
                self.port_combo.addItem(p.device)

    def get_port(self):
        return self.port_combo.currentText().strip()

    # --- Recent projects ---

    def _settings(self):
        return QSettings("FeriteUI", "Designer")

    def _get_recent(self):
        s = self._settings()
        paths = s.value("recent_projects", [])
        if isinstance(paths, str):
            paths = [paths] if paths else []
        return [p for p in paths if os.path.isfile(p)]

    def _add_recent(self, path):
        path = os.path.abspath(path)
        recent = self._get_recent()
        if path in recent:
            recent.remove(path)
        recent.insert(0, path)
        recent = recent[:10]
        self._settings().setValue("recent_projects", recent)
        self._rebuild_recent_menu()

    def _rebuild_recent_menu(self):
        self.recent_menu.clear()
        recent = self._get_recent()
        if not recent:
            act = self.recent_menu.addAction("(no recent projects)")
            act.setEnabled(False)
            return
        for path in recent:
            name = os.path.basename(path)
            parent_dir = os.path.basename(os.path.dirname(path))
            label = f"{name}  ({parent_dir})"
            act = self.recent_menu.addAction(label)
            act.triggered.connect(lambda checked, p=path: self._open_recent(p))

    def _open_recent(self, path):
        if not os.path.isfile(path):
            QMessageBox.warning(self, "Not Found", f"File not found:\n{path}")
            self._rebuild_recent_menu()
            return
        try:
            self.model.load_from_file(path)
            self._set_window_title(path)
            self.resources.refresh()
            self._add_recent(path)
            self.code_editor.load_file()
        except Exception as e:
            QMessageBox.critical(self, "Error", f"Failed to open: {e}")

    def _delete_selected(self):
        if self.model.selected and self.model.selected is not self.model.root:
            self.model.remove_widget(self.model.selected)

    def _bring_to_front(self):
        if self.model.selected:
            self.model.bring_to_front(self.model.selected)

    def _send_to_back(self):
        if self.model.selected:
            self.model.send_to_back(self.model.selected)

    # --- Copy / Paste / Duplicate ---

    def _copy_widget(self):
        self.model.copy_selected()
        if self.model.clipboard:
            self.statusBar().showMessage("Copied", 2000)

    def _paste_widget(self):
        node = self.model.paste_widget()
        if node:
            self.statusBar().showMessage(f"Pasted as '{node.name}'", 2000)

    def _duplicate_widget(self):
        to_dup = [n for n in self.model.selection if n is not self.model.root]
        if not to_dup and self.model.selected and self.model.selected is not self.model.root:
            to_dup = [self.model.selected]
        if not to_dup:
            return
        new_nodes = []
        for node in to_dup:
            d = dict(node.to_dict())
            d["loc_x"] = d.get("loc_x", 0) + 10
            d["loc_y"] = d.get("loc_y", 0) + 10
            base = node.name
            dup_name = base + "_copy"
            n_sfx = 1
            while any(w.name == dup_name for w in self.model.widgets):
                dup_name = f"{base}_copy{n_sfx}"
                n_sfx += 1
            d["name"] = dup_name
            new_node = WidgetNode.from_dict(d)
            parent = node.parent if node.parent else self.model.root
            new_node.parent = parent
            parent.children.append(new_node)
            self.model._counter += 1
            self.model.widgets.append(new_node)
            new_nodes.append(new_node)
        if new_nodes:
            self.model.selection = set(new_nodes)
            self.model.selected = new_nodes[-1]
            self.model.tree_changed.emit()
            self.model.selection_changed.emit()
            self.statusBar().showMessage(f"Duplicated {len(new_nodes)} widget(s)", 2000)

    # --- Alignment helpers ---

    def _selection_secondary(self):
        """Return the set of selected widgets that are NOT the primary anchor."""
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return set()
        others = self.model.selection - {anchor}
        return {n for n in others if n is not self.model.root}

    def _parent_content_origin(self, node):
        """Return (px, py) of the content area origin of node's parent in scene coords."""
        p = node.parent
        if p is None:
            return 0, 0
        px, py = p.abs_pos()
        return px + p.border[3] + p.padding[3], py + p.border[0] + p.padding[0]

    def _align_left(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        ax, _ = anchor.abs_pos()
        for node in self._selection_secondary():
            pcx, _ = self._parent_content_origin(node)
            node.loc_x = ax - pcx
        self.model.notify_changed()

    def _align_right(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        ax, _ = anchor.abs_pos()
        right = ax + anchor.size_w
        for node in self._selection_secondary():
            pcx, _ = self._parent_content_origin(node)
            node.loc_x = right - node.size_w - pcx
        self.model.notify_changed()

    def _align_top(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        _, ay = anchor.abs_pos()
        for node in self._selection_secondary():
            _, pcy = self._parent_content_origin(node)
            node.loc_y = ay - pcy
        self.model.notify_changed()

    def _align_bottom(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        _, ay = anchor.abs_pos()
        bottom = ay + anchor.size_h
        for node in self._selection_secondary():
            _, pcy = self._parent_content_origin(node)
            node.loc_y = bottom - node.size_h - pcy
        self.model.notify_changed()

    def _center_h(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        ax, _ = anchor.abs_pos()
        cx = ax + anchor.size_w / 2
        for node in self._selection_secondary():
            pcx, _ = self._parent_content_origin(node)
            node.loc_x = int(cx - node.size_w / 2 - pcx)
        self.model.notify_changed()

    def _center_v(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        _, ay = anchor.abs_pos()
        cy = ay + anchor.size_h / 2
        for node in self._selection_secondary():
            _, pcy = self._parent_content_origin(node)
            node.loc_y = int(cy - node.size_h / 2 - pcy)
        self.model.notify_changed()

    def _same_width(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        for node in self._selection_secondary():
            node.size_w = anchor.size_w
        self.model.notify_changed()

    def _same_height(self):
        anchor = self.model.selected
        if not anchor or anchor is self.model.root:
            return
        for node in self._selection_secondary():
            node.size_h = anchor.size_h
        self.model.notify_changed()

    def _show_code(self):
        designer_code = generate_designer_fl(self.model)
        preview = (
            f"// === main.fl (user-editable, created once) ===\n\n"
            f"{DEFAULT_MAIN_FL}\n"
            f"// === main.designer.fl (auto-generated on every build) ===\n\n"
            f"{designer_code}"
        )
        dlg = CodeDialog(preview, self)
        dlg.setWindowTitle("Generated Code — main.designer.fl")
        dlg.exec()

    def _new_project(self):
        if QMessageBox.question(self, "New Project",
                                "Discard current project?") == QMessageBox.Yes:
            dlg = DeviceChoiceDialog(self, self.model.device_id)
            if dlg.exec() != QDialog.Accepted:
                return
            self._apply_new_device(dlg.selected_device())

    def _open_project(self):
        path, _ = QFileDialog.getOpenFileName(self, "Open Project", "",
                                               "Ferrite UI (*.fui);;All (*)")
        if path:
            try:
                self.model.load_from_file(path)
                self._set_window_title(path)
                self.resources.refresh()
                self._add_recent(path)
                self.code_editor.load_file()
            except Exception as e:
                QMessageBox.critical(self, "Error", f"Failed to open: {e}")

    def _save_project(self):
        # Sync code editor to model before saving
        if self.code_editor.is_modified():
            self.code_editor.save_file()
        if self.model._path:
            self.model.save_to_file(self.model._path)
            self._add_recent(self.model._path)
            self.statusBar().showMessage("Saved", 3000)
        else:
            self._save_project_as()

    def _save_project_as(self):
        # Sync code editor to model before saving
        if self.code_editor.is_modified():
            self.code_editor.save_file()
        path, _ = QFileDialog.getSaveFileName(self, "Save Project", "",
                                               "Ferrite UI (*.fui)")
        if path:
            self.model.save_to_file(path)
            self._add_recent(path)
            self._set_window_title(path)
            self.statusBar().showMessage("Saved", 3000)

    def _show_device(self):
        dlg = DeviceDialog(self.model, self)
        # Sync port selection from main toolbar
        port = self.get_port()
        if port:
            idx = dlg.port_combo.findText(port)
            if idx >= 0:
                dlg.port_combo.setCurrentIndex(idx)
            else:
                dlg.port_combo.setEditText(port)
        dlg.exec()
        # Sync port back to main toolbar
        dlg_port = dlg._get_port()
        if dlg_port:
            idx = self.port_combo.findText(dlg_port)
            if idx >= 0:
                self.port_combo.setCurrentIndex(idx)
            else:
                self.port_combo.setEditText(dlg_port)

    def _build_and_upload(self):
        """Quick build + upload with progress in status bar."""
        port = self.get_port()
        if not port:
            QMessageBox.warning(self, "Upload", "Select a serial port first.")
            return
        if not self.model._path:
            QMessageBox.warning(self, "Upload", "Save the project first (Ctrl+S).")
            return

        self.statusBar().showMessage("Building...")
        QApplication.processEvents()

        try:
            output_path, build_dir, result = self._build_to_dir()
            if result.returncode != 0:
                QMessageBox.critical(self, "Build Failed", result.stderr or result.stdout)
                self.statusBar().showMessage("Build failed", 5000)
                return
        except Exception as e:
            QMessageBox.critical(self, "Build Error", str(e))
            return

        with open(output_path, "rb") as f:
            data = f.read()

        self.statusBar().showMessage(f"Uploading {len(data)} bytes to {port}...")
        QApplication.processEvents()

        # Upload in a thread to avoid freezing
        import threading
        self._upload_error = None
        self._upload_done = False

        def _upload():
            try:
                import struct as st
                CHUNK_SIZE = 4096
                ser = serial.Serial(port=port, baudrate=115200,
                                    bytesize=serial.EIGHTBITS, parity=serial.PARITY_NONE,
                                    stopbits=serial.STOPBITS_ONE, timeout=10.0)
                # Send writefs header
                header = st.pack("<II", len(data), CHUNK_SIZE)
                msg = bytearray([0x22])
                length = len(header)
                while length > 0x7F:
                    msg.append((length & 0x7F) | 0x80)
                    length >>= 7
                msg.append(length & 0x7F)
                msg.extend(header)
                ser.write(bytes(msg))
                ser.flush()

                # Wait for ACK
                resp = self._wait_pong(ser)
                if not resp:
                    self._upload_error = "Device did not ACK header"
                    ser.close()
                    return

                # Send chunks
                chunk_count = (len(data) + CHUNK_SIZE - 1) // CHUNK_SIZE
                for i in range(chunk_count):
                    chunk = data[i * CHUNK_SIZE:(i + 1) * CHUNK_SIZE]
                    msg = bytearray([0x2A])
                    length = len(chunk)
                    while length > 0x7F:
                        msg.append((length & 0x7F) | 0x80)
                        length >>= 7
                    msg.append(length & 0x7F)
                    msg.extend(chunk)
                    ser.write(bytes(msg))
                    ser.flush()

                    resp = self._wait_pong(ser)
                    if not resp:
                        self._upload_error = f"Device did not ACK chunk {i + 1}/{chunk_count}"
                        ser.close()
                        return

                ser.close()
            except Exception as e:
                self._upload_error = str(e)
            finally:
                self._upload_done = True

        thread = threading.Thread(target=_upload, daemon=True)
        thread.start()

        # Poll for completion without blocking GUI
        from PySide6.QtCore import QTimer
        def _check_upload():
            if not self._upload_done:
                QApplication.processEvents()
                QTimer.singleShot(100, _check_upload)
                return
            if self._upload_error:
                QMessageBox.critical(self, "Upload Failed", self._upload_error)
                self.statusBar().showMessage("Upload failed", 5000)
            else:
                self.statusBar().showMessage("Upload complete! Device restarting...", 5000)
        QTimer.singleShot(100, _check_upload)

    @staticmethod
    def _wait_pong(ser):
        """Wait for a PONG (0x10) response from device."""
        import time
        deadline = time.time() + ser.timeout
        while time.time() < deadline:
            ser.timeout = max(0.1, deadline - time.time())
            b = ser.read(1)
            if not b:
                return False
            if b[0] == 0x10:
                return True
        return False

    def _export_fl(self):
        code = generate_fl(self.model)
        path, _ = QFileDialog.getSaveFileName(self, "Export .fl", "", "Ferrite (*.fl)")
        if path:
            with open(path, "w", encoding="utf-8") as f:
                f.write(code)
            self.statusBar().showMessage(f"Exported to {path}", 3000)

    def _build_to_dir(self):
        """Extract project, build flash.bin, return (output_path, build_dir) or (None, None)."""
        # Save code editor changes before building
        if self.code_editor.is_modified():
            self.code_editor.save_file()

        # Extract all embedded resources to a build dir next to the .fui file
        proj_dir = self.model.project_dir()
        build_dir = os.path.join(proj_dir, ".build") if proj_dir else tempfile.mkdtemp(prefix="ferrite_")
        json_path = self.model.extract_to_dir(build_dir)

        tools_dir = os.path.dirname(os.path.abspath(__file__))
        build_script = os.path.join(tools_dir, "ferrite_build.py")
        output_path = os.path.join(build_dir, "flash.bin")

        result = subprocess.run(
            [sys.executable, build_script, json_path, "-o", output_path],
            capture_output=True, text=True, cwd=tools_dir, timeout=30)

        return output_path, build_dir, result

    def _build_flash(self):
        if not self.model._path:
            QMessageBox.warning(self, "Build", "Save the project first (Ctrl+S).")
            return

        try:
            output_path, build_dir, result = self._build_to_dir()
            if result.returncode == 0:
                # Parse output for stats
                size = os.path.getsize(output_path)
                msg = f"Built flash.bin ({size} bytes, {size / 1024:.1f} KB)"
                self.statusBar().showMessage(msg, 5000)
                # Show full build output
                dlg = CodeDialog(
                    f"Build successful!\n\n"
                    f"Output: {output_path}\n"
                    f"Size: {size} bytes ({size / 1024:.1f} KB)\n\n"
                    f"Project files:\n"
                    f"  main.fl              (user code — edit freely)\n"
                    f"  main.designer.fl     (auto-generated layout)\n"
                    f"  project.json         (auto-generated build config)\n"
                    f"  flash.bin            (built image)\n\n"
                    f"--- Build output ---\n{result.stdout}",
                    self)
                dlg.setWindowTitle("Build Result")
                dlg.exec()
            else:
                QMessageBox.critical(self, "Build Failed",
                                     f"ferrite_build.py failed:\n\n{result.stderr or result.stdout}")
        except FileNotFoundError:
            QMessageBox.critical(self, "Build Error",
                                 f"ferrite_build.py not found at:\n{build_script}")
        except subprocess.TimeoutExpired:
            QMessageBox.critical(self, "Build Error", "Build timed out (30s)")
        except Exception as e:
            QMessageBox.critical(self, "Build Error", str(e))


# ============================================================
# Entry point
# ============================================================

def _apply_fusion_palette(app):
    app.setStyle("Fusion")
    pal = app.palette()

    # Base surfaces
    pal.setColor(pal.ColorRole.Window,          QColor(245, 246, 250))
    pal.setColor(pal.ColorRole.WindowText,      QColor( 30,  30,  40))
    pal.setColor(pal.ColorRole.Base,            QColor(255, 255, 255))
    pal.setColor(pal.ColorRole.AlternateBase,   QColor(235, 237, 245))
    pal.setColor(pal.ColorRole.ToolTipBase,     QColor(255, 255, 220))
    pal.setColor(pal.ColorRole.ToolTipText,     QColor( 30,  30,  40))
    pal.setColor(pal.ColorRole.PlaceholderText, QColor(160, 160, 180))
    pal.setColor(pal.ColorRole.Text,            QColor( 30,  30,  40))
    pal.setColor(pal.ColorRole.BrightText,      QColor(255, 255, 255))

    # Buttons
    pal.setColor(pal.ColorRole.Button,          QColor(225, 228, 240))
    pal.setColor(pal.ColorRole.ButtonText,      QColor( 30,  30,  40))

    # Accent / highlight — steel blue
    pal.setColor(pal.ColorRole.Highlight,       QColor( 66, 133, 244))
    pal.setColor(pal.ColorRole.HighlightedText, QColor(255, 255, 255))

    # Links
    pal.setColor(pal.ColorRole.Link,            QColor( 66, 133, 244))
    pal.setColor(pal.ColorRole.LinkVisited,     QColor(138,  43, 226))

    # Mid-tones used by Fusion for borders / shadow
    pal.setColor(pal.ColorRole.Light,           QColor(255, 255, 255))
    pal.setColor(pal.ColorRole.Midlight,        QColor(210, 213, 225))
    pal.setColor(pal.ColorRole.Mid,             QColor(180, 184, 200))
    pal.setColor(pal.ColorRole.Dark,            QColor(140, 144, 160))
    pal.setColor(pal.ColorRole.Shadow,          QColor( 80,  84, 100))

    app.setPalette(pal)


def main():
    import traceback
    import logging

    log_path = os.path.join(tempfile.gettempdir(), "ferrite_designer.log")
    logging.basicConfig(
        filename=log_path,
        level=logging.ERROR,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    app = QApplication(sys.argv)
    _apply_fusion_palette(app)

    def _excepthook(exc_type, exc_value, exc_tb):
        msg = "".join(traceback.format_exception(exc_type, exc_value, exc_tb))
        logging.error("Unhandled exception:\n%s", msg)
        try:
            QMessageBox.critical(
                None, "Unexpected Error",
                f"An unexpected error occurred and was logged.\n\n"
                f"{exc_type.__name__}: {exc_value}\n\n"
                f"Log: {log_path}",
            )
        except Exception:
            pass
        sys.__excepthook__(exc_type, exc_value, exc_tb)

    sys.excepthook = _excepthook

    try:
        window = DesignerWindow()

        if len(sys.argv) > 1 and os.path.isfile(sys.argv[1]):
            try:
                window.model.load_from_file(sys.argv[1])
                window._set_window_title(sys.argv[1])
                window._add_recent(sys.argv[1])
                window.resources.refresh()
            except Exception as e:
                QMessageBox.critical(window, "Error", f"Failed to open: {e}")
        else:
            dlg = DeviceChoiceDialog(window)
            if dlg.exec() != QDialog.Accepted:
                return
            window._apply_new_device(dlg.selected_device())

        window.show()
        sys.exit(app.exec())

    except Exception:
        msg = traceback.format_exc()
        logging.error("Fatal startup error:\n%s", msg)
        try:
            QMessageBox.critical(
                None, "Fatal Error",
                f"Ferrite Designer failed to start.\n\n{msg}\n\nLog: {log_path}",
            )
        except Exception:
            print(f"FATAL ERROR:\n{msg}", file=sys.stderr)
        raise


if __name__ == "__main__":
    main()
