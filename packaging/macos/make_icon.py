#!/usr/bin/env python3
"""Generate VocalCode.icns from the app's glyph.

Mirrors `icon_rgba()` in vocalcode-app/src/webui.rs — the amber five-bar
waveform on a dark rounded tile — so the Dock icon matches the window icon.
Keep `BARS`/`BAR_W` here identical to the constants there if the mark changes.

(The *menu bar* icon is deliberately not this image: macOS status items take a
monochrome template, which `tray_template_rgba()` in webui.rs renders instead.)

Rendering is supersampled and downsampled for antialiased edges, which matters
far more at 512pt than it does for a 16px tray icon.

Only the standard library is used (zlib for PNG deflate), so packaging does not
depend on Pillow being installed.

Usage: make_icon.py <output.icns>
"""

import os
import struct
import subprocess
import sys
import tempfile
import zlib

# Palette, matching webui.rs.
AMBER = (238, 138, 62, 255)
AMBER_HI = (246, 168, 96, 255)
TILE_TOP = (44, 48, 55, 255)
TILE_BOT = (18, 20, 24, 255)
CLEAR = (0, 0, 0, 0)

# Five bars of differing height — the same waveform the recording indicator
# draws. A plain dot said nothing about what the app is; every comparable
# product's icon reads as "voice" at a glance, and this is ours.
# (x centre, height) as fractions of the tile.
BARS = [(0.22, 0.34), (0.36, 0.60), (0.50, 0.82), (0.64, 0.52), (0.78, 0.28)]
BAR_W = 0.085

# Tile geometry as fractions of the icon edge, taken from the 64px original:
# 3px inset, 14px corner radius.
INSET_F = 3.0 / 64.0
RADIUS_F = 14.0 / 64.0

# macOS icons are not full-bleed; leave a margin so the tile matches the optical
# size of system icons in the Dock.
MARGIN_F = 0.10

SUPERSAMPLE = 4


def _mix(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(4))


def shade(fx, fy, size):
    """Colour of the glyph at a point, in an unpadded tile of `size` px."""
    c = size / 2.0
    inset = INSET_F * size
    rad = RADIUS_F * size

    # Signed distance to a rounded square, the same construction as the Rust.
    dx = abs(fx - c) - (c - inset - rad)
    dy = abs(fy - c) - (c - inset - rad)
    corner = (max(dx, 0.0) ** 2 + max(dy, 0.0) ** 2) ** 0.5 - rad
    inside = min(max(dx, dy), 0.0) + min(corner, 0.0) < 0.0
    if not inside:
        return CLEAR

    u, v = fx / size, fy / size

    # Bars first, so they sit on top of the tile.
    half_w = BAR_W * size / 2.0
    for cx, h in BARS:
        bx = cx * size
        if abs(fx - bx) <= half_w:
            bar_h = h * size
            top, bottom = c - bar_h / 2.0, c + bar_h / 2.0
            # Rounded caps: treat the bar as a capsule.
            if top <= fy <= bottom:
                return _mix(AMBER_HI, AMBER, v)
            cap = top if fy < top else bottom
            if ((fx - bx) ** 2 + (fy - cap) ** 2) ** 0.5 <= half_w:
                return _mix(AMBER_HI, AMBER, v)

    # Tile gradient, lighter at the top like every icon it sits beside.
    return _mix(TILE_TOP, TILE_BOT, min(1.0, (u * 0.25 + v * 0.9)))


def render(size):
    """Render `size`x`size` RGBA bytes, supersampled then box-filtered."""
    margin = int(round(size * MARGIN_F))
    inner = size - 2 * margin
    ss = SUPERSAMPLE
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            r = g = b = a = 0
            for sy in range(ss):
                for sx in range(ss):
                    # Sample position mapped into the unpadded tile.
                    fx = (x + (sx + 0.5) / ss) - margin
                    fy = (y + (sy + 0.5) / ss) - margin
                    if 0 <= fx < inner and 0 <= fy < inner:
                        pr, pg, pb, pa = shade(fx, fy, inner)
                    else:
                        pr, pg, pb, pa = CLEAR
                    # Weight colour by coverage so edges blend correctly.
                    r += pr * pa
                    g += pg * pa
                    b += pb * pa
                    a += pa
            n = ss * ss
            if a == 0:
                row += bytes(4)
            else:
                row += bytes((r // a, g // a, b // a, a // n))
        rows.append(bytes(row))
    return rows


def write_png(path, size, rows):
    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + r for r in rows)  # filter type 0 per scanline
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    out = sys.argv[1]

    # (base point size, scale) pairs iconutil expects in an .iconset.
    variants = [
        (16, 1), (16, 2), (32, 1), (32, 2), (128, 1),
        (128, 2), (256, 1), (256, 2), (512, 1), (512, 2),
    ]

    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "VocalCode.iconset")
        os.makedirs(iconset)
        # Render each pixel size once; several variants share one (e.g. 32x32
        # and 16x16@2x are both 32px).
        cache = {}
        for base, scale in variants:
            px = base * scale
            if px not in cache:
                cache[px] = render(px)
            suffix = "@2x" if scale == 2 else ""
            name = f"icon_{base}x{base}{suffix}.png"
            write_png(os.path.join(iconset, name), px, cache[px])

        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", out],
            check=True,
        )
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
