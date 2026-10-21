"""Draw the sample relay-button icons (64x64 PNG) used by relay_board.

The background is BTN_BG from main.fl (RGB565 0x2966 = RGB 40,44,52), so the
icons blend into the button face. Run from this directory:

    python make_icons.py
"""
from PIL import Image, ImageDraw

BG = (40, 44, 52)
SIZE = 64
FG = (230, 230, 230)
YELLOW = (255, 210, 60)
CYAN = (80, 200, 255)


def canvas():
    img = Image.new("RGB", (SIZE, SIZE), BG)
    return img, ImageDraw.Draw(img)


def bulb():
    img, d = canvas()
    d.ellipse((14, 6, 50, 42), fill=YELLOW)
    d.rectangle((24, 40, 40, 50), fill=YELLOW)
    for y in (44, 50, 56):
        d.rectangle((22, y, 42, y + 3), fill=FG)
    return img


def fan():
    img, d = canvas()
    c = SIZE // 2
    for a in (0, 120, 240):
        d.pieslice((6, 6, 58, 58), a, a + 50, fill=CYAN)
    d.ellipse((c - 7, c - 7, c + 7, c + 7), fill=FG)
    return img


def plug():
    img, d = canvas()
    d.rectangle((18, 22, 46, 46), fill=FG)
    d.rectangle((24, 8, 28, 22), fill=FG)
    d.rectangle((36, 8, 40, 22), fill=FG)
    d.rectangle((29, 46, 35, 58), fill=FG)
    return img


def pump():
    img, d = canvas()
    d.polygon([(32, 6), (14, 36), (32, 58), (50, 36)], fill=CYAN)
    d.ellipse((14, 26, 50, 58), fill=CYAN)
    d.ellipse((22, 36, 30, 44), fill=FG)
    return img


for name, draw in (("bulb", bulb), ("fan", fan), ("plug", plug), ("pump", pump)):
    draw().save(f"{name}.png")
    print(f"{name}.png")
