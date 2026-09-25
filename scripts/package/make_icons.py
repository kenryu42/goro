#!/usr/bin/env python3
"""Draw Goro's icon and write it in every size the packages need.

Usage: make_icons.py <out dir>

Writes goro-<size>.png (16…1024), goro.ico (Windows) and goro.iconset/ (for macOS
`iconutil`). Pure Python (zlib only), so it runs anywhere CI does.
"""
import struct
import sys
import zlib
from pathlib import Path

BG = (13, 17, 23)
REMOVED = (248, 81, 73)
ADDED = (63, 185, 80)
LINE = (125, 133, 144)


def rounded_rect(x, y, x0, y0, x1, y1, r):
    """Whether (x, y) is inside a rectangle with rounded corners of radius r."""
    if not (x0 <= x < x1 and y0 <= y < y1):
        return False
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def pixel(u, v):
    """RGBA at normalized coordinates (u, v) in [0, 1)."""
    # Background: a rounded square with a small margin, like platform app icons.
    if not rounded_rect(u, v, 0.06, 0.06, 0.94, 0.94, 0.2):
        return (0, 0, 0, 0)
    # Three diff lines: context, a removal, an addition.
    bars = [
        (0.30, LINE, 0.62),
        (0.47, REMOVED, 0.70),
        (0.64, ADDED, 0.74),
    ]
    for top, color, right in bars:
        if rounded_rect(u, v, 0.22, top, right, top + 0.09, 0.045):
            return (*color, 255)
    # A gutter mark beside the removal and addition.
    if rounded_rect(u, v, 0.16, 0.47, 0.19, 0.73, 0.015):
        return (*LINE, 255)
    return (*BG, 255)


def render(size, samples=4):
    """RGBA rows, supersampled for smooth edges."""
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(samples):
                for sx in range(samples):
                    u = (x + (sx + 0.5) / samples) / size
                    v = (y + (sy + 0.5) / samples) / size
                    r, g, b, a = pixel(u, v)
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
            n = samples * samples
            alpha = acc[3] / n
            if acc[3]:
                row += bytes([round(acc[0] / acc[3]), round(acc[1] / acc[3]), round(acc[2] / acc[3])])
            else:
                row += bytes([0, 0, 0])
            row.append(round(alpha))
        rows.append(bytes(row))
    return rows


def png(size):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\x00" + row for row in render(size))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico(images):
    """An .ico holding PNG-encoded images (supported since Windows Vista)."""
    header = struct.pack("<HHH", 0, 1, len(images))
    entries = b""
    data = b""
    offset = 6 + 16 * len(images)
    for size, blob in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        data += blob
        offset += len(blob)
    return header + entries + data


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    pngs = {}
    for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        pngs[size] = png(size)
        (out / f"goro-{size}.png").write_bytes(pngs[size])
    (out / "goro.ico").write_bytes(ico([(s, pngs[s]) for s in (16, 24, 32, 48, 64, 256)]))
    iconset = out / "goro.iconset"
    iconset.mkdir(exist_ok=True)
    for size in (16, 32, 128, 256, 512):
        (iconset / f"icon_{size}x{size}.png").write_bytes(pngs[size])
        (iconset / f"icon_{size}x{size}@2x.png").write_bytes(pngs[size * 2])
    print(f"icons written to {out}")


if __name__ == "__main__":
    main()
