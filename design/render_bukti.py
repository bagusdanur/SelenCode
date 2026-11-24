#!/usr/bin/env python3
"""Render dump sel TestBackend (dari examples/render_welcome.rs) ke PNG.
Sel braille digambar sebagai titik-titik (seperti font terminal yang
bagus), sel lain digambar dengan DejaVu Sans Mono. Palet sama seperti
moonlit_mockup.py."""
import sys
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONTB = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SIZE = 25
CW = 15.05
LH = 30
BG = (13, 10, 26)
SILVER = (229, 231, 235)

BRAILLE_BITS = [(0, 0, 0x01), (1, 0, 0x08), (0, 1, 0x02), (1, 1, 0x10),
                (0, 2, 0x04), (1, 2, 0x20), (0, 3, 0x40), (1, 3, 0x80)]


def draw_braille(d, ch, x, y, color):
    bits = ord(ch) - 0x2800
    if bits == 0:
        return
    for dx, dy, bit in BRAILLE_BITS:
        if bits & bit:
            cx = x + (dx + 0.5) * (CW / 2)
            cy = y + (dy + 0.5) * (LH / 4)
            r = 3.1
            d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=color)


def main(dump_path, png_path):
    # split HANYA pada \n — splitlines() ikut memotong pada \x1e (separator sel).
    lines = open(dump_path, encoding="utf-8").read().split("\n")
    w, h = map(int, lines[0].split())
    img = Image.new("RGB", (int(w * CW) + 28, h * LH + 28), BG)
    d = ImageDraw.Draw(img)
    f = ImageFont.truetype(FONT, SIZE)
    fb = ImageFont.truetype(FONTB, SIZE)
    for ry, row in enumerate(lines[1:1 + h]):
        y = 14 + ry * LH
        for rx, cell in enumerate(row.split("\x1e")):
            parts = cell.split("\x1f")
            if len(parts) != 3:
                continue
            sym, fg, bold = parts
            color = SILVER if fg == "-" else tuple(int(v) for v in fg.split(","))
            x = 14 + rx * CW
            if len(sym) == 1 and 0x2800 <= ord(sym) <= 0x28FF:
                draw_braille(d, sym, x, y, color)
            elif sym and sym != " ":
                d.text((x, y), sym, font=fb if bold == "1" else f, fill=color)
    img.save(png_path)
    print(png_path, img.size)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
