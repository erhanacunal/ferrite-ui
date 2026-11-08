#!/usr/bin/env python3
"""make_assets.py — generates the tablet demo's assets into assets/.

    python make_assets.py [--icons DIR]

- Icons: SVGs from an icon folder (default E:\\ico\\images, or the
  FERRITE_ICONS environment variable), recoloured to white and rasterized
  with an alpha channel (PySide6 QtSvg). They come from the Visual Studio
  Image Library, which may not be redistributed, so assets/ is git-ignored
  and only this script and the icon names are in the repository.
- Fonts: fonts/Ubuntu-R.ttf (repository root), 2 bpp anti-aliased, through
  tools/ferrite_font_converter.py.
- Gallery pictures: procedural landscapes drawn here (PIL).
- Sounds: synthesized here (numpy), 11025 Hz mono.

Everything is deterministic, so re-running gives the same files.
"""

import argparse
import math
import os
import re
import subprocess
import sys
import wave
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
OUT = HERE / "assets"

# ---------------------------------------------------------------------------
# Icons: (output name, SVG name, size px)
# ---------------------------------------------------------------------------
ICONS = [
    # App glyphs (white, drawn on a coloured circle by the program)
    ("app_clock", "Time", 52),
    ("app_calc", "Calculator", 52),
    ("app_paint", "Brush", 52),
    ("app_monitor", "Performance", 52),
    ("app_gallery", "Image", 52),
    ("app_music", "Media", 52),
    ("app_notes", "Note", 52),
    ("app_settings", "Settings", 52),
    # Status bar
    ("st_volume", "Volume", 22),
    ("st_mute", "AudioMute", 22),
    ("st_bright", "Brightness", 22),
    # Quick settings / controls
    ("qs_bright", "Brightness", 30),
    ("qs_volume", "Volume", 30),
    ("qs_dark", "DarkTheme", 30),
    ("qs_restart", "Restart", 30),
    ("qs_perf", "Performance", 30),
    ("ui_back", "Previous", 30),
    ("ui_add", "Add", 30),
    ("ui_delete", "Delete", 30),
    ("ui_eraser", "Eraser", 30),
    ("ui_undo", "Undo", 30),
    ("ui_flag", "Flag", 30),
    ("ui_reset", "Refresh", 30),
    ("ui_check", "Checkmark", 30),
    ("ui_play", "Play", 40),
    ("ui_pause", "Pause", 40),
    ("ui_prev", "GlyphLeft", 30),
    ("ui_next", "GlyphRight", 30),
    ("ui_left", "GlyphLeft", 40),
    ("ui_right", "GlyphRight", 40),
]

# ---------------------------------------------------------------------------
# Fonts: (output name, size, ranges)
# ---------------------------------------------------------------------------
TEXT_RANGES = "32-126,0xB0,0xB7,0xD7,0xF7,0x2022,0x2026,0x2212"
FONTS = [
    ("f16", 16, TEXT_RANGES),
    ("f20", 20, TEXT_RANGES),
    ("f26", 26, TEXT_RANGES),
    ("f36", 36, TEXT_RANGES),
    ("f60", 60, "32-126,0xB0,0xD7,0xF7,0x2212"),
    ("f120", 120, "32,0x2E,0x30-0x3A"),
]


def make_icons(icon_dir: Path):
    from PySide6.QtCore import QByteArray, QRectF
    from PySide6.QtGui import QGuiApplication, QImage, QPainter, QColor
    from PySide6.QtSvg import QSvgRenderer

    app = QGuiApplication.instance() or QGuiApplication(sys.argv[:1])
    _ = app
    for name, svg_name, size in ICONS:
        src = icon_dir / f"{svg_name}.svg"
        if not src.exists():
            sys.exit(f"icon {src} not found (set --icons or FERRITE_ICONS)")
        svg = re.sub(r"#[0-9A-Fa-f]{6}", "#FFFFFF", src.read_text(encoding="utf-8"))
        img = QImage(size, size, QImage.Format_ARGB32)
        img.fill(QColor(0, 0, 0, 0))
        p = QPainter(img)
        p.setRenderHint(QPainter.Antialiasing)
        QSvgRenderer(QByteArray(svg.encode())).render(p, QRectF(0, 0, size, size))
        p.end()
        img.save(str(OUT / f"{name}.png"))
    print(f"icons: {len(ICONS)}")


def make_fonts():
    conv = ROOT / "tools" / "ferrite_font_converter.py"
    ttf = ROOT / "fonts" / "Ubuntu-R.ttf"
    for i, (name, size, ranges) in enumerate(FONTS):
        subprocess.run([sys.executable, str(conv), "resource", str(ttf), str(size),
                        "--bpp", "2", "--font-id", str(i + 1), "-r", ranges,
                        "-o", str(OUT / f"{name}.font")], check=True,
                       stdout=subprocess.DEVNULL)
    print(f"fonts: {len(FONTS)}")


# ---------------------------------------------------------------------------
# Gallery: procedural landscapes (flat bands, so the RLE stays small)
# ---------------------------------------------------------------------------
GW, GH = 960, 560
TW, TH = 288, 168


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def sky(img, top, bottom, horizon):
    d = ImageDraw.Draw(img)
    for y in range(img.height):
        t = min(1.0, y / max(1, horizon))
        d.line([(0, y), (img.width, y)], fill=lerp(top, bottom, t))


def ridge(rng, width, base, amp, rough, step=24):
    """Midpoint-displacement ridge line: list of (x, y)."""
    n = width // step + 2
    ys = [base + rng.uniform(-amp, amp) for _ in range(n)]
    for _ in range(2):
        ys = [ys[0]] + [(ys[i - 1] + ys[i] * 2 + ys[i + 1]) / 4 for i in range(1, n - 1)] + [ys[-1]]
    pts = []
    for i in range(n - 1):
        x0, x1 = i * step, (i + 1) * step
        y0, y1 = ys[i], ys[i + 1]
        mid = (y0 + y1) / 2 + rng.uniform(-rough, rough)
        pts += [(x0, y0), ((x0 + x1) / 2, mid)]
    pts.append(((n - 1) * step, ys[-1]))
    return pts


def mountains(img, rng, layers):
    d = ImageDraw.Draw(img)
    for base, amp, rough, color in layers:
        pts = ridge(rng, img.width, base, amp, rough)
        d.polygon(pts + [(img.width, img.height), (0, img.height)], fill=color)


def scene_dusk(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (24, 30, 72), (250, 150, 110), 330)
    d = ImageDraw.Draw(img)
    d.ellipse([600, 210, 700, 310], fill=(255, 214, 150))
    mountains(img, rng, [(300, 50, 9, (128, 84, 120)), (360, 44, 8, (90, 58, 100)),
                         (420, 30, 6, (58, 38, 78)), (480, 20, 4, (34, 22, 52))])
    return img


def scene_ocean(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (255, 120, 80), (255, 200, 120), 300)
    d = ImageDraw.Draw(img)
    d.ellipse([400, 210, 560, 370], fill=(255, 240, 200))
    pts = ridge(rng, GW, 292, 8, 4)
    d.polygon(pts + [(GW, 300), (0, 300)], fill=(150, 70, 90))
    for y in range(300, GH):
        t = (y - 300) / (GH - 300)
        d.line([(0, y), (GW, y)], fill=lerp((60, 70, 140), (16, 24, 70), t))
    for i in range(26):
        y = 310 + i * 9
        w = 40 + i * 14
        d.line([(480 - w / 2 + rng.uniform(-20, 20), y), (480 + w / 2, y)], fill=(255, 200, 150), width=2)
    return img


def scene_aurora(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (6, 10, 28), (16, 40, 70), GH)
    d = ImageDraw.Draw(img)
    for _ in range(140):
        x, y = rng.uniform(0, GW), rng.uniform(0, 330)
        d.point((x, y), fill=(220, 230, 255))
    band = Image.new("L", (GW, GH))
    bd = ImageDraw.Draw(band)
    for k in range(3):
        pts = [(x, 140 + k * 40 + 40 * math.sin(x / 120 + k)) for x in range(0, GW + 20, 20)]
        bd.line(pts, fill=200 - k * 50, width=26)
    band = band.filter(ImageFilter.GaussianBlur(14))
    green = Image.new("RGB", (GW, GH), (60, 255, 170))
    img = Image.composite(green, img, band.point(lambda v: v * 0.55))
    mountains(img, rng, [(380, 30, 12, (20, 40, 60)), (440, 26, 10, (10, 24, 40)), (500, 14, 6, (4, 12, 24))])
    return img


def scene_dunes(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (70, 150, 230), (200, 225, 245), 300)
    d = ImageDraw.Draw(img)
    d.ellipse([140, 70, 230, 160], fill=(255, 250, 225))
    cols = [(236, 190, 120), (222, 166, 96), (204, 142, 76), (182, 118, 60)]
    for i, c in enumerate(cols):
        base = 320 + i * 60
        pts = [(x, base + 26 * math.sin(x / (160 + i * 30) + i * 1.7)) for x in range(0, GW + 16, 16)]
        d.polygon(pts + [(GW, GH), (0, GH)], fill=c)
    return img


def scene_forest(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (190, 220, 210), (240, 236, 210), 360)
    d = ImageDraw.Draw(img)
    layers = [(300, (150, 180, 160)), (360, (100, 140, 120)), (430, (60, 100, 84)), (500, (30, 64, 52))]
    for base, col in layers:
        x = -20
        while x < GW + 20:
            h = rng.uniform(60, 140) * (base / 400)
            w = h * 0.42
            d.polygon([(x, base), (x + w / 2, base - h), (x + w, base)], fill=col)
            x += w * rng.uniform(0.45, 0.8)
        d.rectangle([0, base, GW, GH], fill=col)
    return img


def scene_city(rng):
    img = Image.new("RGB", (GW, GH))
    sky(img, (14, 10, 40), (90, 40, 110), 420)
    d = ImageDraw.Draw(img)
    d.ellipse([720, 60, 790, 130], fill=(240, 236, 220))
    for layer, (col, win) in enumerate([((44, 30, 80), (130, 110, 170)), ((22, 14, 44), (255, 200, 120))]):
        x = 0
        while x < GW:
            w = rng.uniform(40, 90)
            h = rng.uniform(120, 300) - layer * 40
            top = GH - 60 - h
            d.rectangle([x, top, x + w, GH], fill=col)
            for wy in range(int(top) + 12, GH - 70, 18):
                for wx in range(int(x) + 8, int(x + w) - 8, 14):
                    if rng.random() < 0.35:
                        d.rectangle([wx, wy, wx + 5, wy + 8], fill=win)
            x += w + rng.uniform(2, 10)
    d.rectangle([0, GH - 60, GW, GH], fill=(10, 8, 22))
    return img


SCENES = [("dusk", scene_dusk), ("ocean", scene_ocean), ("aurora", scene_aurora),
          ("dunes", scene_dunes), ("forest", scene_forest), ("city", scene_city)]


def make_gallery():
    import random
    for i, (name, fn) in enumerate(SCENES):
        img = fn(random.Random(1000 + i))
        img.save(OUT / f"pic_{name}.png")
        img.resize((TW, TH), Image.LANCZOS).save(OUT / f"th_{name}.png")
    print(f"gallery: {len(SCENES)} pictures")


# ---------------------------------------------------------------------------
# Wallpapers
# ---------------------------------------------------------------------------
# The home screen's background: the vertical gradient plus three translucent
# blobs, pre-composited into one opaque picture. As widgets (a gradient root
# and three alpha circles) every restore behind a sliding page re-blended up
# to 1.5 screens of blob pixels; as a picture it is one copy.
SW, SH = 1280, 800
# (top, bottom) RGB565, as WALL_TOP/WALL_BOT and WALL2_TOP/WALL2_BOT in ui.fl.
WALLS = [("wall1", 0x08A6, 0x38CA), ("wall2", 0x0965, 0x1209)]
# (x, y, size, radius, RGB565, alpha 0..255): BLOB1..3 in ui.fl, the boxes
# build_wallpaper() used to place. As widgets their boxes were cut to the
# screen before the corners were rounded (radius at most half the cut box's
# shorter side), so blobs crossing the edge were rounded rects, not circles:
# `rounded_box` draws them that way, to look as before.
BLOBS = [(-260, 300, 760, 380, 0x4A3C, 70), (860, -260, 640, 320, 0xEA53, 52), (700, 560, 420, 210, 0x15D4, 40)]


def expand565(c):
    """RGB565 -> 8-bit channels, as the exynos framebuffer expands them."""
    r, g, b = (c >> 11) & 0x1F, (c >> 5) & 0x3F, c & 0x1F
    return (r << 3 | r >> 2, g << 2 | g >> 4, b << 3 | b >> 2)


def lerp565(c1, c2, t, n):
    """`lerp_rgb565` (ferrite-core lcd.rs): the gradient's row colours."""
    t8 = min(t, n) * 256 // n if n else 0
    out = 0
    for shift, mask in ((11, 0x1F), (5, 0x3F), (0, 0x1F)):
        a, b = (c1 >> shift) & mask, (c2 >> shift) & mask
        out |= ((a * (256 - t8) + b * t8) >> 8) << shift
    return out


def make_wallpaper(top, bottom):
    import numpy as np
    rows = np.array([expand565(lerp565(top, bottom, y, SH - 1)) for y in range(SH)], dtype=np.float64)
    img = np.repeat(rows[:, None, :], SW, axis=1)
    ys, xs = np.mgrid[0:SH, 0:SW]
    for x, y, size, radius, color, alpha in BLOBS:
        x0, y0, x1, y1 = max(x, 0), max(y, 0), min(x + size, SW), min(y + size, SH)
        r = min(radius, (x1 - x0) / 2, (y1 - y0) / 2)
        # Signed distance to the rounded rect, coverage over one pixel.
        cx, cy, hw, hh = (x0 + x1) / 2, (y0 + y1) / 2, (x1 - x0) / 2, (y1 - y0) / 2
        qx = np.abs(xs + 0.5 - cx) - (hw - r)
        qy = np.abs(ys + 0.5 - cy) - (hh - r)
        dist = np.hypot(np.maximum(qx, 0), np.maximum(qy, 0)) + np.minimum(np.maximum(qx, qy), 0) - r
        a = (np.clip(0.5 - dist, 0, 1) * alpha / 255)[..., None]
        img = img * (1 - a) + np.array(expand565(color), dtype=np.float64) * a
    # To RGB565 with a 4x4 ordered dither (the blobs' blends fall between
    # 565 steps), stored as 8-bit values that ferrite_img truncates back to
    # exactly these 565 values.
    bayer = (np.array([[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]]) + 0.5) / 16
    dither = np.tile(bayer, (SH // 4 + 1, SW // 4 + 1))[:SH, :SW]
    out = np.empty((SH, SW, 3), dtype=np.uint8)
    for ch, bits in enumerate((5, 6, 5)):
        levels = np.arange(1 << bits)
        steps = (levels << (8 - bits)) | (levels >> (2 * bits - 8))
        v = img[..., ch]
        # The step at or below v, then up one by the fraction past it: a value
        # that is already a step (the plain gradient rows) stays exact.
        q = np.clip(np.searchsorted(steps, v, side="right") - 1, 0, len(steps) - 2)
        frac = (v - steps[q]) / (steps[q + 1] - steps[q])
        q = q + (frac > dither)
        out[..., ch] = steps[q]
    return Image.fromarray(out, "RGB")


def make_wallpapers():
    for name, top, bottom in WALLS:
        make_wallpaper(top, bottom).save(OUT / f"{name}.png")
    print(f"wallpapers: {len(WALLS)}")


# ---------------------------------------------------------------------------
# Sounds
# ---------------------------------------------------------------------------
RATE = 11025


def write_wav(path, samples):
    s = np.clip(samples, -1, 1)
    data = (s * 32000).astype("<i2").tobytes()
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(data)


def note_hz(n):
    return 440.0 * 2 ** ((n - 69) / 12)


def pluck(n, dur, vol=0.3, harm=(1, 0.5, 0.25), decay=4.0):
    t = np.arange(int(dur * RATE)) / RATE
    f = note_hz(n)
    s = sum(a * np.sin(2 * np.pi * f * (k + 1) * t) for k, a in enumerate(harm))
    env = np.exp(-decay * t) * np.minimum(1, t * 200)
    return vol * s * env


def pad(n, dur, vol=0.12):
    t = np.arange(int(dur * RATE)) / RATE
    f = note_hz(n)
    s = np.sin(2 * np.pi * f * t) + 0.5 * np.sin(2 * np.pi * f * 1.004 * t) + 0.3 * np.sin(2 * np.pi * f * 2 * t)
    env = np.minimum(1, t * 3) * np.minimum(1, (dur - t) * 3)
    return vol * s * env


def mix(track, part, at):
    i = int(at * RATE)
    end = min(len(track), i + len(part))
    track[i:end] += part[: end - i]


def song(bpm, chords, arp, bass=True, bars_per_chord=1, lead=None):
    beat = 60 / bpm
    bar = beat * 4
    total = bar * bars_per_chord * len(chords)
    out = np.zeros(int(total * RATE) + RATE)
    t = 0.0
    for chord in chords:
        for _ in range(bars_per_chord):
            for n in chord:
                mix(out, pad(n, bar), t)
            if bass:
                for b in range(4):
                    mix(out, pluck(chord[0] - 12, beat, 0.35, (1, 0.3), 5), t + b * beat)
            for k in range(8):
                n = chord[arp[k % len(arp)] % len(chord)] + 12 * (arp[k % len(arp)] // len(chord))
                mix(out, pluck(n + 12, beat / 2 * 1.5, 0.16, (1, 0.4, 0.2), 6), t + k * beat / 2)
            t += bar
    if lead:
        lt = 0.0
        for n, beats in lead:
            if n:
                mix(out, pluck(n, beats * beat, 0.22, (1, 0.6, 0.3, 0.1), 2.5), lt)
            lt += beats * beat
    out = out[: int(total * RATE)]
    # Short crossfade so the loop point doesn't click.
    fade = int(0.02 * RATE)
    out[:fade] *= np.linspace(0, 1, fade)
    out[-fade:] *= np.linspace(1, 0, fade)
    return out / max(1e-6, np.abs(out).max()) * 0.85


def make_sounds():
    # Am F C G — a calm loop
    write_wav(OUT / "track1.wav", song(96, [[57, 60, 64], [53, 57, 60], [48, 52, 55], [55, 59, 62]],
                                       [0, 1, 2, 3, 2, 1, 0, 2],
                                       lead=[(76, 2), (74, 1), (72, 1), (69, 4), (72, 2), (74, 2), (76, 2), (79, 2)]))
    # Dm Bb F C — brighter
    write_wav(OUT / "track2.wav", song(116, [[50, 53, 57], [46, 50, 53], [53, 57, 60], [48, 52, 55]],
                                       [0, 2, 1, 3, 0, 2, 4, 3],
                                       lead=[(74, 1), (77, 1), (81, 2), (79, 2), (77, 1), (76, 1), (74, 2),
                                             (72, 2), (74, 4), (0, 2)]))
    # Cmaj7 Am7 Fmaj7 G — lounge
    write_wav(OUT / "track3.wav", song(84, [[48, 52, 55, 59], [45, 48, 52, 55], [41, 45, 48, 52], [43, 47, 50, 53]],
                                       [0, 1, 2, 3, 4, 3, 2, 1], bass=True))
    t = np.arange(int(0.012 * RATE)) / RATE
    write_wav(OUT / "click.wav", 0.5 * np.sin(2 * np.pi * 2400 * t) * np.exp(-t * 500))
    chime = np.zeros(int(0.9 * RATE))
    mix(chime, pluck(84, 0.6, 0.5, (1, 0.3, 0.1), 5), 0)
    mix(chime, pluck(91, 0.7, 0.5, (1, 0.3, 0.1), 5), 0.14)
    write_wav(OUT / "chime.wav", chime)
    print("sounds: 5")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--icons", default=os.environ.get("FERRITE_ICONS", r"E:\ico\images"),
                    help="folder with the icon SVGs")
    args = ap.parse_args()
    OUT.mkdir(exist_ok=True)
    make_icons(Path(args.icons))
    make_fonts()
    make_gallery()
    make_wallpapers()
    make_sounds()


if __name__ == "__main__":
    main()
