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
        QDialogButtonBox, QTabWidget, QPlainTextEdit,
    )
    from PySide6.QtCore import Qt, QRectF, QPointF, Signal, QSize, QSettings, QRegularExpression
    from PySide6.QtGui import (
        QColor, QPainter, QPen, QBrush, QFont, QAction,
        QPainterPath, QKeySequence, QSyntaxHighlighter, QTextCharFormat,
        QFontDatabase,
    )
except ImportError:
    print("PySide6 is required: pip install PySide6")
    sys.exit(1)


# ============================================================
# Constants
# ============================================================

SCREEN_W, SCREEN_H = 800, 480

KIND_BASE = 0
KIND_LABEL = 1
KIND_BUTTON = 2
KIND_PROGRESS = 3
KIND_SLIDER = 4
KIND_CHECKBOX = 5
KIND_RADIO = 6
KIND_DROPDOWN = 9

KIND_NAMES = ["Base", "Label", "Button", "Progress", "Slider", "Checkbox", "Radio", "Dropdown"]
KIND_VALUES = [KIND_BASE, KIND_LABEL, KIND_BUTTON, KIND_PROGRESS, KIND_SLIDER, KIND_CHECKBOX, KIND_RADIO, KIND_DROPDOWN]
KIND_PREFIXES = {
    KIND_BASE: "panel",
    KIND_LABEL: "lbl",
    KIND_BUTTON: "btn",
    KIND_PROGRESS: "progress",
    KIND_SLIDER: "slider",
    KIND_CHECKBOX: "cb",
    KIND_RADIO: "radio",
    KIND_DROPDOWN: "dropdown",
}


def kind_name(kind):
    if kind in KIND_VALUES:
        return KIND_NAMES[KIND_VALUES.index(kind)]
    return "?"

HANDLE_SIZE = 8
CANVAS_MARGIN = 40
GRID_SIZE = 10


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


# ============================================================
# Resource preview caches
# ============================================================

import struct as _struct
from PySide6.QtGui import QImage, QPixmap


class GfxFontPreview:
    """Parses Adafruit GFX binary font data for QPainter rendering."""

    def __init__(self, binary):
        if len(binary) < 6:
            raise ValueError("font binary too short")
        self.first, self.last = _struct.unpack_from("<HH", binary, 0)
        self.y_advance = binary[4]
        # font_id at byte 5 — not needed for rendering
        glyph_count = self.last - self.first + 1
        self.glyphs = []  # (bitmap_offset, w, h, x_advance, x_offset, y_offset)
        offset = 6
        for _ in range(glyph_count):
            if offset + 7 > len(binary):
                break
            bm_off = _struct.unpack_from("<H", binary, offset)[0]
            w = binary[offset + 2]
            h = binary[offset + 3]
            x_adv = binary[offset + 4]
            x_off = binary[offset + 5] if binary[offset + 5] < 128 else binary[offset + 5] - 256
            y_off = binary[offset + 6] if binary[offset + 6] < 128 else binary[offset + 6] - 256
            self.glyphs.append((bm_off, w, h, x_adv, x_off, y_off))
            offset += 7
        self.bitmap = binary[offset:]

    def text_width(self, text):
        w = 0
        for ch in text:
            idx = ord(ch) - self.first
            if 0 <= idx < len(self.glyphs):
                w += self.glyphs[idx][3]  # x_advance
        return w

    def line_height(self):
        return self.y_advance

    def draw_text(self, painter, x, y, text, color):
        """Draw text at (x, y=baseline) using 1-bit glyph bitmaps."""
        pen = painter.pen()
        painter.setPen(Qt.NoPen)
        brush = QBrush(color)
        for ch in text:
            idx = ord(ch) - self.first
            if idx < 0 or idx >= len(self.glyphs):
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
        # Find font resource
        for fres in self.model.fonts:
            if fres.get("font_id") != font_id:
                continue
            try:
                if "combined_b64" in fres:
                    data = base64.b64decode(fres["combined_b64"])
                elif "header_b64" in fres and "data_b64" in fres:
                    data = base64.b64decode(fres["header_b64"]) + base64.b64decode(fres["data_b64"])
                else:
                    data = b""
                if len(data) >= 6:
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

        self.text = ""
        self.font_id = 0
        self.text_align = 0

        self.visible = True
        self.enabled = True
        self.clickable = False
        self.value = 0
        self.checked = False
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
            "text": self.text, "font_id": self.font_id, "text_align": self.text_align,
            "visible": self.visible, "enabled": self.enabled,
            "clickable": self.clickable,
            "value": self.value, "checked": self.checked, "image_id": self.image_id,
            "on_click": self.on_click, "on_tap": self.on_tap, "on_paint": self.on_paint,
        }

    @staticmethod
    def from_dict(d):
        n = WidgetNode(d["name"], d.get("kind", 0))
        for key in ("loc_x", "loc_y", "size_w", "size_h", "bg_color", "border_color",
                     "border_radius", "text_color", "press_color", "text", "font_id",
                     "text_align", "visible", "enabled", "clickable", "value", "checked",
                     "image_id", "on_click", "on_tap", "on_paint"):
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

    def __init__(self):
        super().__init__()
        self.root = WidgetNode("root", KIND_BASE)
        self.root.size_w = SCREEN_W
        self.root.size_h = SCREEN_H
        self.root.bg_color = 0x0000
        self.widgets = [self.root]
        self.selected = None
        self._counter = 0
        self._path = None
        self.snap_to_grid = False

        # Project resources — all binary data embedded as base64 in .fui
        self.fonts = []     # [{"name": str, "font_id": int, "header_b64": str, "data_b64": str}]
        self.images = []    # [{"name": str, "image_id": int, "source_name": str, "mode": "auto", "max_colors": 256, "data_b64": str}]
        self.programs = []  # [{"name": str, "exec_mode": "ram"|"flash", "source_b64": str}]
        self._res_gen = 0   # bumped when fonts/images change, for cache invalidation
        self.include_dirs = []  # extra include dirs for compilation
        self.exec_mode = "flash"  # default exec_mode for the main program
        self.render_mode = "dirty"  # "dirty" (partial update) or "buffered" (full redraw)
        self.main_fl = DEFAULT_MAIN_FL  # user code — embedded in .fui

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

    def select(self, node):
        self.selected = node
        self.selection_changed.emit()

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

    def clear(self):
        self.root = WidgetNode("root", KIND_BASE)
        self.root.size_w = SCREEN_W
        self.root.size_h = SCREEN_H
        self.widgets = [self.root]
        self.selected = None
        self._counter = 0
        self.fonts = []
        self.images = []
        self.programs = []
        self.include_dirs = []
        self.exec_mode = "flash"
        self.render_mode = "dirty"
        self.main_fl = DEFAULT_MAIN_FL
        self.tree_changed.emit()
        self.selection_changed.emit()

    def save_to_file(self, path):
        data = {
            "version": 2,
            "counter": self._counter,
            "widgets": [w.to_dict() for w in self.dfs_order()],
            "main_fl": base64.b64encode(self.main_fl.encode("utf-8")).decode("ascii"),
            "fonts": self.fonts[:],
            "images": self.images[:],
            "programs": self.programs[:],
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
        self.widgets = [n for n, _ in order] if order else [self.root]
        self._counter = data.get("counter", len(self.widgets))

        # main.fl — embedded or from disk (backward compat)
        if "main_fl" in data:
            self.main_fl = base64.b64decode(data["main_fl"]).decode("utf-8")
        else:
            self.main_fl = DEFAULT_MAIN_FL

        # Resources — migrate old path-based format to embedded
        self.fonts = []
        for f_entry in data.get("fonts", []):
            if "header_b64" in f_entry or "combined_b64" in f_entry:
                self.fonts.append(f_entry)
            elif "header" in f_entry:
                # Old format: read files from disk and embed
                proj_dir = os.path.dirname(os.path.abspath(path))
                try:
                    h_path = os.path.join(proj_dir, f_entry["header"])
                    d_path = os.path.join(proj_dir, f_entry["data"])
                    h_b64 = base64.b64encode(open(h_path, "rb").read()).decode("ascii")
                    d_b64 = base64.b64encode(open(d_path, "rb").read()).decode("ascii")
                    self.fonts.append({
                        "name": f_entry["name"], "font_id": f_entry["font_id"],
                        "header_b64": h_b64, "data_b64": d_b64,
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

        self.include_dirs = data.get("include_dirs", [])
        self.exec_mode = data.get("exec_mode", "flash")
        self.render_mode = data.get("render_mode", "dirty")
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

        # Fonts — extract to header + data binaries for ferrite_build.py
        font_entries = []
        for font in self.fonts:
            import struct as st
            name = font["name"]
            header_file = f"{name}_header.bin"
            data_file = f"{name}_data.bin"

            if "combined_b64" in font:
                # Combined binary from .h conversion: split into header + bitmap
                combined = base64.b64decode(font["combined_b64"])
                first = int.from_bytes(combined[0:2], 'little')
                last = int.from_bytes(combined[2:4], 'little')
                glyph_count = last - first + 1
                split_at = 6 + glyph_count * 7
                with open(os.path.join(build_dir, header_file), "wb") as f:
                    f.write(combined[:split_at])
                with open(os.path.join(build_dir, data_file), "wb") as f:
                    f.write(combined[split_at:])
            else:
                # Separate header + data binaries
                with open(os.path.join(build_dir, header_file), "wb") as f:
                    f.write(base64.b64decode(font["header_b64"]))
                with open(os.path.join(build_dir, data_file), "wb") as f:
                    f.write(base64.b64decode(font["data_b64"]))

            font_entries.append({
                "name": name, "font_id": font["font_id"],
                "header": header_file, "data": data_file,
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

        # project.json
        dirs = list(self.include_dirs)
        if "." not in dirs:
            dirs.insert(0, ".")
        proj = {
            "version": 2,
            "screen": [SCREEN_W, SCREEN_H],
            "include_dirs": dirs,
            "render_mode": self.render_mode,
            "fonts": font_entries,
            "images": image_entries,
            "programs": [
                {"name": "main", "source": "main.fl", "exec_mode": self.exec_mode}
            ] + prog_entries,
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
                lh = gfx_font.line_height()
                content_w = w - pad_l - pad_r - 4
                content_h = h - pad_t - pad_b
                # Horizontal alignment
                if node.text_align == 1:
                    tx = pad_l + 2 + (content_w - tw) // 2
                elif node.text_align == 2:
                    tx = pad_l + 2 + content_w - tw
                else:
                    tx = pad_l + 2
                # Vertical center — y is baseline
                ty = pad_t + (content_h + lh) // 2 - 2
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

        # Dim if not visible
        if not node.visible:
            painter.fillRect(QRectF(0, 0, w, h), QColor(128, 128, 128, 100))

    def mousePressEvent(self, event):
        if event.button() == Qt.LeftButton:
            self.model.select(self.node)
            if self.node is not self.model.root:
                self._dragging = True
                self._drag_start = event.scenePos()
                self._orig_loc = (self.node.loc_x, self.node.loc_y)
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event):
        if self._dragging:
            delta = event.scenePos() - self._drag_start
            new_x = self._orig_loc[0] + int(delta.x())
            new_y = self._orig_loc[1] + int(delta.y())
            self.node.loc_x = self.model.snap(new_x)
            self.node.loc_y = self.model.snap(new_y)
            self.model.notify_changed()
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event):
        self._dragging = False
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
        self.setSceneRect(-CANVAS_MARGIN, -CANVAS_MARGIN,
                          SCREEN_W + 2 * CANVAS_MARGIN,
                          SCREEN_H + 2 * CANVAS_MARGIN)
        model.tree_changed.connect(self.rebuild)
        model.changed.connect(self.refresh)
        model.selection_changed.connect(self.update_selection)
        self.rebuild()

    def drawBackground(self, painter, rect):
        # Dark gray background outside the screen
        painter.fillRect(rect, QColor(50, 50, 50))
        # Screen boundary
        painter.setPen(QPen(QColor(100, 100, 100), 1))
        painter.drawRect(QRectF(-1, -1, SCREEN_W + 2, SCREEN_H + 2))

    def rebuild(self):
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

    def mousePressEvent(self, event):
        item = self.itemAt(event.scenePos(), self.views()[0].transform() if self.views() else __import__('PySide6.QtGui', fromlist=['QTransform']).QTransform())
        if item is None:
            self.model.select(None)
        super().mousePressEvent(event)


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
            text = f"{node.name} ({kind_name(node.kind)}, {node.size_w}x{node.size_h})"
            if parent_item is None:
                item = QTreeWidgetItem(self, [text])
            else:
                item = QTreeWidgetItem(parent_item, [text])
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

    def __init__(self, value=0):
        super().__init__()
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
        color = QColorDialog.getColor(qc, self, "Pick Color")
        if color.isValid():
            self._value = qcolor_to_rgb565(color)
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
        btn = ColorButton()
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
        add_font_btn.clicked.connect(self._add_font)
        rm_font_btn = QPushButton("- Font")
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
        add_img_btn.clicked.connect(self._add_image)
        rm_img_btn = QPushButton("- Image")
        rm_img_btn.clicked.connect(self._rm_image)
        ib.addWidget(add_img_btn)
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
        add_prog_btn.clicked.connect(self._add_program)
        rm_prog_btn = QPushButton("- Program")
        rm_prog_btn.clicked.connect(self._rm_program)
        pb.addWidget(add_prog_btn)
        pb.addWidget(rm_prog_btn)
        pb.addStretch()
        layout.addLayout(pb)

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
        self.render_combo.addItems(["dirty", "buffered"])
        self.render_combo.currentTextChanged.connect(lambda v: setattr(model, 'render_mode', v))
        em_row.addWidget(self.render_combo)
        em_row.addStretch()
        layout.addLayout(em_row)

        layout.addStretch()

        model.tree_changed.connect(self.refresh)

    def refresh(self):
        self.font_list.clear()
        for f in self.model.fonts:
            if "combined_b64" in f:
                size = len(base64.b64decode(f["combined_b64"]))
                src = ".h"
            else:
                size = len(base64.b64decode(f.get("header_b64", ""))) + len(base64.b64decode(f.get("data_b64", "")))
                src = ".bin"
            QTreeWidgetItem(self.font_list, [
                f.get("name", ""), str(f.get("font_id", "")),
                f"{size} B ({src})"])
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
        self.include_edit.setText(", ".join(self.model.include_dirs))
        idx = self.exec_combo.findText(self.model.exec_mode)
        if idx >= 0:
            self.exec_combo.setCurrentIndex(idx)
        idx = self.render_combo.findText(self.model.render_mode)
        if idx >= 0:
            self.render_combo.setCurrentIndex(idx)

    def _add_font(self):
        path, _ = QFileDialog.getOpenFileName(
            self, "Font File (.h or .bin)",  "",
            "Adafruit GFX Header (*.h);;Binary header+data (*.bin);;All (*)")
        if not path:
            return

        name = os.path.splitext(os.path.basename(path))[0]
        used_ids = {f.get("font_id", 0) for f in self.model.fonts}
        fid = 1
        while fid in used_ids:
            fid += 1

        if path.lower().endswith(".h"):
            # Auto-convert Adafruit GFX .h → combined binary
            try:
                combined = gfx_header_to_bin(path, fid)
                # Split into header (first 6 bytes) and rest (glyphs + bitmap)
                # But for our combined format, header_b64 = first 6 bytes + glyphs, data_b64 = bitmap
                # Actually, ferrite_build.py expects header (first+last+yAdvance + glyphs) and data (bitmap) separately
                # Then it injects font_id at byte 5 and concatenates them
                # Simpler: store as combined, extract_to_dir writes the combined binary
                header_b64 = base64.b64encode(combined).decode("ascii")
                self.model.fonts.append({
                    "name": name, "font_id": fid,
                    "combined_b64": header_b64,
                })
                self.model._res_gen += 1
                self.refresh()
                self.model.notify_changed()
            except Exception as e:
                QMessageBox.critical(self, "Font Conversion Error",
                                     f"Failed to parse {os.path.basename(path)}:\n\n{e}")
        else:
            # Binary: ask for header + data pair
            header_b64 = base64.b64encode(open(path, "rb").read()).decode("ascii")
            data, _ = QFileDialog.getOpenFileName(
                self, "Font Bitmap (.bin)", os.path.dirname(path), "Binary (*.bin)")
            if not data:
                return
            data_b64 = base64.b64encode(open(data, "rb").read()).decode("ascii")
            self.model.fonts.append({
                "name": name, "font_id": fid,
                "header_b64": header_b64, "data_b64": data_b64,
            })
            self.model._res_gen += 1
            self.refresh()
            self.model.notify_changed()

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
            "mode": "auto", "max_colors": 256,
            "data_b64": data_b64,
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
    """Parse an Adafruit GFX C header (.h) and produce the combined flash binary.

    Flash binary format:
      [0..2]  first: u16 LE
      [2..4]  last: u16 LE
      [4]     y_advance: u8
      [5]     font_id: u8
      [6..]   glyph table (7 bytes per glyph: bitmapOffset u16LE, w, h, xAdv, xOff i8, yOff i8)
      [6+N*7..] bitmap data (1-bit packed, MSB first)
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

    # Glyph array — strip C comments before parsing to avoid ' in comments breaking the regex
    gm = re.search(
        r"const\s+GFXglyph\s+\w+Glyphs\[\]\s+(?:PROGMEM\s*)?=\s*\{(.+?)\};",
        src, re.DOTALL)
    if not gm:
        raise ValueError("Glyph array not found (expected: const GFXglyph ...Glyphs[] = {...})")

    glyph_text = re.sub(r"//[^\n]*", "", gm.group(1))  # strip line comments
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
        raise ValueError(f"Expected {glyph_count} glyphs (first=0x{first:02X}, last=0x{last:02X}), found {len(glyphs)}")

    # Build binary
    out = bytearray()
    out.extend(struct.pack("<HH", first, last))  # first, last (u16 LE)
    out.append(y_advance)                          # y_advance (u8)
    out.append(font_id & 0xFF)                     # font_id (u8)

    # Glyph table
    for g in glyphs[:glyph_count]:
        offset, w, h, x_adv, x_off, y_off = g
        out.extend(struct.pack("<H", offset & 0xFFFF))
        out.append(w & 0xFF)
        out.append(h & 0xFF)
        out.append(x_adv & 0xFF)
        out.append(x_off & 0xFF)  # i8 as u8
        out.append(y_off & 0xFF)  # i8 as u8

    # Bitmap data
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
        save_btn.clicked.connect(self.save_file)
        tb.addWidget(save_btn)
        reload_btn = QPushButton("Revert")
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
        save_btn = btns.addButton("Save .fl", QDialogButtonBox.ActionRole)
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
        ping_btn.clicked.connect(self._ping)
        btn_row.addWidget(ping_btn)
        restart_btn = QPushButton("Restart")
        restart_btn.clicked.connect(self._restart)
        btn_row.addWidget(restart_btn)
        meminfo_btn = QPushButton("MemInfo")
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
        build_upload_btn.setStyleSheet("font-weight: bold; padding: 8px;")
        build_upload_btn.clicked.connect(self._build_and_writefs)
        ug_layout.addWidget(build_upload_btn)

        file_row = QHBoxLayout()
        writefs_btn = QPushButton("Upload flash.bin...")
        writefs_btn.clicked.connect(self._writefs_file)
        file_row.addWidget(writefs_btn)
        exec_btn = QPushButton("Execute .bin...")
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
            self.log.moveCursor(self.log.textCursor().End)
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

        # Layout: left panel (tree + props) | center tabs (Design/Code) | right (resources)
        left_splitter = QSplitter(Qt.Vertical)
        left_splitter.addWidget(self.tree)
        left_splitter.addWidget(self.props)
        left_splitter.setStretchFactor(0, 1)
        left_splitter.setStretchFactor(1, 2)

        self.center_tabs = QTabWidget()
        self.center_tabs.addTab(self.canvas, "Design")
        self.center_tabs.addTab(self.code_editor, "Code (main.fl)")
        self.center_tabs.currentChanged.connect(self._on_tab_changed)

        right_splitter = QSplitter(Qt.Vertical)
        right_splitter.addWidget(self.resources)

        main_splitter = QSplitter(Qt.Horizontal)
        main_splitter.addWidget(left_splitter)
        main_splitter.addWidget(self.center_tabs)
        main_splitter.addWidget(right_splitter)
        main_splitter.setStretchFactor(0, 0)
        main_splitter.setStretchFactor(1, 1)
        main_splitter.setStretchFactor(2, 0)
        main_splitter.setSizes([300, 680, 280])
        self.setCentralWidget(main_splitter)

        self._build_toolbar()
        self._build_menu()
        self.statusBar().showMessage("Ready")

    def _build_toolbar(self):
        tb = self.addToolBar("Widgets")
        tb.setMovable(False)
        for idx, kind in enumerate(KIND_VALUES):
            act = QAction(f"+ {KIND_NAMES[idx]}", self)
            act.triggered.connect(lambda checked, k=kind: self.model.add_widget(k))
            tb.addAction(act)
        tb.addSeparator()
        del_act = QAction("Delete", self)
        del_act.setShortcut(QKeySequence.Delete)
        del_act.triggered.connect(self._delete_selected)
        tb.addAction(del_act)
        tb.addSeparator()
        snap_act = QAction("Snap Grid", self)
        snap_act.setCheckable(True)
        snap_act.toggled.connect(lambda v: setattr(self.model, 'snap_to_grid', v))
        tb.addAction(snap_act)
        tb.addSeparator()
        code_act = QAction("Generate Code", self)
        code_act.setShortcut(QKeySequence("Ctrl+G"))
        code_act.triggered.connect(self._show_code)
        tb.addAction(code_act)
        tb.addSeparator()
        build_act = QAction("Build Flash", self)
        build_act.setShortcut(QKeySequence("Ctrl+B"))
        build_act.triggered.connect(self._build_flash)
        tb.addAction(build_act)
        upload_act = QAction("Upload", self)
        upload_act.setShortcut(QKeySequence("Ctrl+U"))
        upload_act.triggered.connect(self._build_and_upload)
        tb.addAction(upload_act)

        # Program-wide serial port selector
        tb.addSeparator()
        tb.addWidget(QLabel(" Port: "))
        self.port_combo = QComboBox()
        self.port_combo.setEditable(True)
        self.port_combo.setMinimumWidth(100)
        tb.addWidget(self.port_combo)
        refresh_act = QAction("Scan", self)
        refresh_act.triggered.connect(self._refresh_ports)
        tb.addAction(refresh_act)
        self._refresh_ports()

    def _build_menu(self):
        mb = self.menuBar()

        file_menu = mb.addMenu("&File")
        new_act = file_menu.addAction("&New")
        new_act.setShortcut(QKeySequence.New)
        new_act.triggered.connect(self._new_project)
        open_act = file_menu.addAction("&Open...")
        open_act.setShortcut(QKeySequence.Open)
        open_act.triggered.connect(self._open_project)
        save_act = file_menu.addAction("&Save")
        save_act.setShortcut(QKeySequence.Save)
        save_act.triggered.connect(self._save_project)
        saveas_act = file_menu.addAction("Save &As...")
        saveas_act.setShortcut(QKeySequence("Ctrl+Shift+S"))
        saveas_act.triggered.connect(self._save_project_as)
        file_menu.addSeparator()
        self.recent_menu = file_menu.addMenu("&Recent Projects")
        self._rebuild_recent_menu()
        file_menu.addSeparator()
        export_act = file_menu.addAction("&Export .fl...")
        export_act.setShortcut(QKeySequence("Ctrl+E"))
        export_act.triggered.connect(self._export_fl)
        file_menu.addSeparator()
        build_act2 = file_menu.addAction("&Build Flash Image")
        build_act2.setShortcut(QKeySequence("Ctrl+B"))
        build_act2.triggered.connect(self._build_flash)
        file_menu.addSeparator()
        quit_act = file_menu.addAction("&Quit")
        quit_act.setShortcut(QKeySequence.Quit)
        quit_act.triggered.connect(self.close)

        edit_menu = mb.addMenu("&Edit")
        del_act = edit_menu.addAction("&Delete Widget")
        del_act.setShortcut(QKeySequence.Delete)
        del_act.triggered.connect(self._delete_selected)

        device_menu = mb.addMenu("&Device")
        device_act = device_menu.addAction("&Device Manager...")
        device_act.setShortcut(QKeySequence("Ctrl+D"))
        device_act.triggered.connect(self._show_device)
        device_menu.addSeparator()
        build_upload_act = device_menu.addAction("Build && &Upload")
        build_upload_act.setShortcut(QKeySequence("Ctrl+U"))
        build_upload_act.triggered.connect(self._build_and_upload)

        view_menu = mb.addMenu("&View")
        fit_act = view_menu.addAction("&Fit to Window")
        fit_act.setShortcut(QKeySequence("Ctrl+0"))
        fit_act.triggered.connect(lambda: self.canvas.fitInView(
            self.scene.sceneRect(), Qt.KeepAspectRatio))

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
            self.setWindowTitle(f"Ferrite UI Designer — {os.path.basename(path)}")
            self.resources.refresh()
            self._add_recent(path)
            self.code_editor.load_file()
        except Exception as e:
            QMessageBox.critical(self, "Error", f"Failed to open: {e}")

    def _delete_selected(self):
        if self.model.selected and self.model.selected is not self.model.root:
            self.model.remove_widget(self.model.selected)

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
            self.model.clear()
            self.model._path = None
            self.code_editor.load_file()  # loads DEFAULT_MAIN_FL from model
            self.center_tabs.setCurrentIndex(0)
            self.setWindowTitle("Ferrite UI Designer")

    def _open_project(self):
        path, _ = QFileDialog.getOpenFileName(self, "Open Project", "",
                                               "Ferrite UI (*.fui);;All (*)")
        if path:
            try:
                self.model.load_from_file(path)
                self.setWindowTitle(f"Ferrite UI Designer — {os.path.basename(path)}")
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
            self.setWindowTitle(f"Ferrite UI Designer — {os.path.basename(path)}")
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

def main():
    app = QApplication(sys.argv)
    app.setStyle("Fusion")

    window = DesignerWindow()

    if len(sys.argv) > 1 and os.path.isfile(sys.argv[1]):
        try:
            window.model.load_from_file(sys.argv[1])
            window.setWindowTitle(f"Ferrite UI Designer — {os.path.basename(sys.argv[1])}")
            window._add_recent(sys.argv[1])
            window.resources.refresh()
        except Exception as e:
            QMessageBox.critical(window, "Error", f"Failed to open: {e}")

    window.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
