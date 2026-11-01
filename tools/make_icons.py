#!/usr/bin/env python3
"""web/icon.svg と同じ電源マークから PNG のアイコンを作る(標準ライブラリだけで動く)。

    python3 tools/make_icons.py

出力: web/icon-192.png, web/icon-512.png, web/icon-maskable-512.png, web/apple-touch-icon.png
"""
import math
import struct
import zlib
from pathlib import Path

BLUE = (0x25, 0x63, 0xEB)
WHITE = (0xFF, 0xFF, 0xFF)
SS = 3  # 1 ピクセルを SS×SS に分けて縁をなめらかにする


def in_glyph(x, y):
    """512×512 の座標系で、電源マークの線の上にあるか(icon.svg のパスと同じ形)。"""
    # 縦線 M256 120 v136(太さ 40、端は丸い)
    t = min(max(y - 120, 0), 136)
    if math.hypot(x - 256, y - (120 + t)) <= 20:
        return True
    # 弧 M166 178 a140 140 0 1 0 180 0(中心は弦の 107.2 下、上に隙間のある円)
    cx, cy, r = 256, 178 + math.sqrt(140**2 - 90**2), 140
    d = math.hypot(x - cx, y - cy)
    if abs(d - r) <= 20 and not _in_gap(x, y, cx, cy):
        return True
    for ex in (166, 346):  # 弧の端の丸み
        if math.hypot(x - ex, y - 178) <= 20:
            return True
    return False


def _in_gap(x, y, cx, cy):
    """弧が途切れている上側(中心から見て真上から ±asin(90/140) の範囲)か。"""
    angle = math.degrees(math.atan2(x - cx, cy - y))  # 真上が 0 度
    return abs(angle) < math.degrees(math.asin(90 / 140))


def in_rounded_square(x, y, radius=112):
    qx, qy = abs(x - 256) - (256 - radius), abs(y - 256) - (256 - radius)
    return max(qx, qy) <= 0 or math.hypot(max(qx, 0), max(qy, 0)) <= radius


def render(size, rounded, glyph_scale):
    rows = []
    for py in range(size):
        row = bytearray([0])  # フィルターなし
        for px in range(size):
            bg = glyph = 0
            for sy in range(SS):
                for sx in range(SS):
                    x = (px + (sx + 0.5) / SS) * 512 / size
                    y = (py + (sy + 0.5) / SS) * 512 / size
                    bg += in_rounded_square(x, y) if rounded else 1
                    gx, gy = 256 + (x - 256) / glyph_scale, 256 + (y - 256) / glyph_scale
                    glyph += in_glyph(gx, gy)
            n = SS * SS
            mix = glyph / n
            r, g, b = (round(BLUE[i] * (1 - mix) + WHITE[i] * mix) for i in range(3))
            row += bytes((r, g, b, round(255 * bg / n)))
        rows.append(bytes(row))
    return rows


def write_png(path, size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b"")
    path.write_bytes(png)


if __name__ == "__main__":
    out = Path(__file__).resolve().parent.parent / "web"
    # maskable は端が切り取られるので、マークを内側の安全域(中央 80%)に収める。iOS は自分で角を丸める。
    for name, size, rounded, scale in [
        ("icon-192.png", 192, True, 1.0),
        ("icon-512.png", 512, True, 1.0),
        ("icon-maskable-512.png", 512, False, 0.7),
        ("apple-touch-icon.png", 180, False, 0.8),
    ]:
        write_png(out / name, size, render(size, rounded, scale))
        print("wrote", out / name)
