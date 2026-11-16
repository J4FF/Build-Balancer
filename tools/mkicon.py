#!/usr/bin/env python3
"""Draws the logo (docs/img/logo.svg) as the app's icon for Windows: gui/res/app.ico.

    python tools/mkicon.py

Needs Pillow (pip install pillow) -- only for this script; the build takes the .ico as it is.
The shapes are the logo's, on its 128 x 128 grid; change both files together.
"""
import os
import struct
import sys
from io import BytesIO

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
TOP, BOTTOM = (0x2D, 0x74, 0xC8), (0x18, 0x4E, 0x94)


def draw(size):
    """the logo at size x size pixels (drawn four times as large, then scaled down)"""
    n = size * 4
    k = n / 128.0

    def box(x, y, w, h):
        return [x * k, y * k, (x + w) * k - 1, (y + h) * k - 1]

    # the background: a gradient inside a rounded square
    grad = Image.new("RGBA", (n, n))
    px = ImageDraw.Draw(grad)
    for y in range(n):
        t = y / (n - 1)
        px.line([(0, y), (n, y)], fill=tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,))
    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box(0, 0, 128, 128), radius=28 * k, fill=255)
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)

    # the right load is lighter in colour: 70 % white over the background
    right = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    ImageDraw.Draw(right).rounded_rectangle(box(74, 28, 32, 32), radius=7 * k, fill=(255, 255, 255, 178))
    img = Image.alpha_composite(img, right)

    d = ImageDraw.Draw(img)
    white = (255, 255, 255, 255)
    d.rounded_rectangle(box(22, 28, 32, 32), radius=7 * k, fill=white)
    d.rounded_rectangle(box(14, 64, 100, 10), radius=5 * k, fill=white)
    tri = [(64 * k, 77 * k), (81 * k, 99 * k), (47 * k, 99 * k)]
    d.polygon(tri, fill=white)
    d.line(tri + [tri[0]], fill=white, width=round(6 * k), joint="curve")
    for x, y in tri:
        d.ellipse([x - 3 * k, y - 3 * k, x + 3 * k, y + 3 * k], fill=white)
    return img.resize((size, size), Image.LANCZOS)


def bmp_entry(img):
    """an icon image the old way: a 32-bit bitmap, bottom row first, and an empty mask"""
    w, h = img.size
    head = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    rows = b"".join(img.crop((0, y, w, y + 1)).tobytes("raw", "BGRA") for y in range(h - 1, -1, -1))
    mask = bytes(((w + 31) // 32) * 4) * h
    return head + rows + mask


def png_entry(img):
    b = BytesIO()
    img.save(b, "PNG", optimize=True)
    return b.getvalue()


def main():
    # small sizes as bitmaps (every program reads them), the large one as PNG (it stays small)
    images = [(s, png_entry(draw(s)) if s >= 256 else bmp_entry(draw(s))) for s in SIZES]
    out = struct.pack("<HHH", 0, 1, len(images))
    at = 6 + 16 * len(images)
    for s, data in images:
        out += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), at)
        at += len(data)
    out += b"".join(data for _, data in images)
    path = os.path.join(ROOT, "gui", "res", "app.ico")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(out)
    print("wrote", path, len(out), "bytes")
    if len(sys.argv) > 1:  # python tools/mkicon.py FILE.png: a picture to look at
        draw(256).save(sys.argv[1])
        print("wrote", sys.argv[1])


if __name__ == "__main__":
    main()
