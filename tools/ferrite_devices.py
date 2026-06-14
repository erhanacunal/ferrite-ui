"""Device definition loader for ferrite-ui tools.

Device specs live in tools/devices/*.json — one file per supported board.
Adding a new device means adding a JSON file there; no tool code changes.

Each JSON is normalized into a flat dict compatible with the historical
``DEVICE_SPECS`` shape used by ferrite_designer (name, screen tuple,
color_model, color_count, render_mode, image_max_colors, root_bg,
text_color) plus the structured extras (render_modes, widgets, caps,
bsp, shape).

NOTE: ``caps`` entries mirror per-board Rust constants and must be kept
in sync by hand:
  - caps.alpha / caps.image_alpha  <->  LcdBackend::HAS_ALPHA in the
    BSP's lcd backend (ferrite-core/src/lcd.rs trait)
  - caps.audio / caps.video        <->  Platform::CAPS bits
    (ferrite-core/src/platform.rs)
"""

import json
import sys
from pathlib import Path

DEVICES_DIR = Path(__file__).resolve().parent / "devices"

DEFAULT_DEVICE_ID = "nextion"

# Embedded fallback used when tools/devices/ is missing or yields nothing
# (e.g. a partial checkout). Mirrors the JSON files — keep in sync.
_FALLBACK = {
    "nextion": {
        "name": "Nextion NX8048K070 / GD32",
        "bsp": "bsp/nextion",
        "screen": (800, 480),
        "shape": "rect",
        "color_model": "RGB565",
        "color_count": 65536,
        "render_mode": "dirty",
        "render_modes": ["dirty", "buffered"],
        "image_max_colors": 256,
        "root_bg": 0x0000,
        "text_color": 0xFFFF,
        "widgets": ["base", "label", "button", "progress", "slider",
                    "checkbox", "radio", "input", "gauge", "dropdown", "image"],
        "caps": {"alpha": False, "audio": False, "video": False,
                 "image_alpha": False},
        "fs_base": 0x1000,
    },
    "epaper": {
        "name": "LilyGo T5 e-Paper S3",
        "bsp": "bsp/epaper",
        "screen": (960, 540),
        "shape": "rect",
        "color_model": "4-bit grayscale",
        "color_count": 16,
        "render_mode": "epaper",
        "render_modes": ["epaper"],
        "image_max_colors": 16,
        "root_bg": 0xFFFF,
        "text_color": 0x0000,
        "widgets": ["base", "label", "button", "progress", "slider",
                    "checkbox", "radio", "input", "gauge", "dropdown", "image"],
        "caps": {"alpha": False, "audio": False, "video": False,
                 "image_alpha": False},
        "fs_base": 0x1000,
    },
}

_cache = None


def _parse_color(value, default):
    """Accept int or hex string ("0xFFFF") color values."""
    if value is None:
        return default
    if isinstance(value, int):
        return value
    return int(str(value), 0)


def _normalize(raw):
    """Convert a devices/*.json dict into the flat legacy-compatible shape."""
    screen = raw.get("screen", {})
    color = raw.get("color", {})
    defaults = raw.get("defaults", {})
    render_modes = raw.get("render_modes", ["dirty", "buffered"])
    return {
        "name": raw.get("name", raw.get("id", "?")),
        "bsp": raw.get("bsp", ""),
        "screen": (int(screen.get("width", 800)), int(screen.get("height", 480))),
        "shape": screen.get("shape", "rect"),
        "color_model": color.get("model", "RGB565"),
        "color_count": int(color.get("count", 65536)),
        "render_mode": raw.get("default_render_mode", render_modes[0]),
        "render_modes": list(render_modes),
        "image_max_colors": int(color.get("image_max_colors", 256)),
        "root_bg": _parse_color(defaults.get("root_bg"), 0x0000),
        "text_color": _parse_color(defaults.get("text_color"), 0xFFFF),
        "widgets": list(raw.get("widgets", [])),
        "caps": dict(raw.get("caps", {})),
        # Flash address of the resource filesystem header (must match the
        # board's Platform::FS_BASE in Rust). writefs targets this and the
        # build tool bakes it into the (absolute) resource offsets.
        "fs_base": _parse_color(raw.get("flash", {}).get("fs_base"), 0x1000),
    }


def load_devices():
    """Return {device_id: spec} from tools/devices/*.json (cached).

    Falls back to the embedded specs when the directory is missing or no
    file parses. Malformed files are skipped with a warning.
    """
    global _cache
    if _cache is not None:
        return _cache

    devices = {}
    if DEVICES_DIR.is_dir():
        for path in sorted(DEVICES_DIR.glob("*.json")):
            try:
                raw = json.loads(path.read_text(encoding="utf-8"))
                dev_id = raw.get("id", path.stem)
                devices[dev_id] = _normalize(raw)
            except (OSError, ValueError, KeyError, TypeError) as e:
                print(f"warning: skipping device file {path.name}: {e}",
                      file=sys.stderr)

    if not devices:
        devices = {k: dict(v) for k, v in _FALLBACK.items()}

    _cache = devices
    return devices


def device_spec(device_id):
    """Spec for device_id, falling back to the default device."""
    devices = load_devices()
    if device_id in devices:
        return devices[device_id]
    if DEFAULT_DEVICE_ID in devices:
        return devices[DEFAULT_DEVICE_ID]
    return next(iter(devices.values()))


def default_device():
    return device_spec(DEFAULT_DEVICE_ID)


def infer_device_id(screen, render_mode):
    """Guess the device for legacy project files without a "device" key."""
    if render_mode == "epaper":
        return "epaper"
    if list(screen or []) == [960, 540]:
        return "epaper"
    if list(screen or []) == [480, 480]:
        return "tdo_y13"
    return DEFAULT_DEVICE_ID
