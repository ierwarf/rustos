#!/usr/bin/env python3
import math
import os
import struct
import zlib

SIZE = 100
HOTSPOT = SIZE // 2
MAX_DISTANCE = 60
BLUE = (54, 148, 255)
WHITE = (236, 250, 255)
OUT_DIR = os.path.join(os.path.dirname(os.path.dirname(__file__)), "assets", "cursor")


def clamp_u8(value):
    value = max(0, min(255, int(round(value))))
    return 0 if value < 7 else value


def add_gaussian(alpha, x, y, cx, cy, radius, strength):
    dx = x - cx
    dy = y - cy
    falloff = math.exp(-(dx * dx + dy * dy) / (2.0 * radius * radius))
    return alpha + strength * falloff


def sprite_channels(distance):
    pixels = []
    moving = distance > 0
    tail_length = 0.0 if not moving else min(HOTSPOT - 10.0, distance * 0.5)

    for y in range(SIZE):
        for x in range(SIZE):
            blue_alpha = 0.0
            white_alpha = 0.0

            if moving:
                # The base sprite points right, so the trail grows left from the hotspot.
                along = HOTSPOT - x
                if 0.0 <= along <= tail_length:
                    t = along / max(tail_length, 1.0)
                    perp = abs(y - HOTSPOT)
                    core_width = 4.2 * (1.0 - t) + 0.85 * t
                    glow_width = 10.8 * (1.0 - t) + 2.0 * t
                    taper = (1.0 - t) ** 1.55
                    head_notch = 1.0 - math.exp(-(along * along) / (2.0 * 5.5 * 5.5))
                    tail_alpha = taper * head_notch
                    blue_alpha += 84.0 * tail_alpha * math.exp(
                        -(perp * perp) / (2.0 * glow_width * glow_width)
                    )
                    blue_alpha += 104.0 * tail_alpha * math.exp(
                        -(perp * perp) / (2.0 * core_width * core_width)
                    )

                    wave = math.sin(t * math.pi * 1.35) * 2.6 * (1.0 - t)
                    ribbon_perp = abs((y - HOTSPOT) - wave)
                    ribbon_width = 1.0 * (1.0 - t) + 0.55
                    blue_alpha += 54.0 * tail_alpha * math.exp(
                        -(ribbon_perp * ribbon_perp) / (2.0 * ribbon_width * ribbon_width)
                    )

            blue_alpha = add_gaussian(blue_alpha, x, y, HOTSPOT, HOTSPOT, 17.0, 28.0)
            blue_alpha = add_gaussian(blue_alpha, x, y, HOTSPOT, HOTSPOT, 10.5, 76.0)
            blue_alpha = add_gaussian(blue_alpha, x, y, HOTSPOT, HOTSPOT, 5.0, 128.0)
            white_alpha = add_gaussian(white_alpha, x, y, HOTSPOT, HOTSPOT, 2.0, 240.0)
            white_alpha = add_gaussian(white_alpha, x, y, HOTSPOT, HOTSPOT, 0.75, 255.0)

            if not moving:
                blue_alpha *= 0.8
                white_alpha *= 0.8

            pixels.append((clamp_u8(blue_alpha), clamp_u8(white_alpha)))
    return pixels


def write_png(path, channels):
    rows = []
    for y in range(SIZE):
        row = bytearray([0])
        for x in range(SIZE):
            blue_alpha, white_alpha = channels[y * SIZE + x]
            white_mix = white_alpha / 255.0
            alpha = max(blue_alpha, white_alpha)
            r = round(BLUE[0] * (1.0 - white_mix) + WHITE[0] * white_mix)
            g = round(BLUE[1] * (1.0 - white_mix) + WHITE[1] * white_mix)
            b = round(BLUE[2] * (1.0 - white_mix) + WHITE[2] * white_mix)
            row.extend((clamp_u8(r), clamp_u8(g), clamp_u8(b), alpha))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = bytearray()
    png.extend(b"\x89PNG\r\n\x1a\n")
    png.extend(chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)))
    png.extend(chunk(b"IDAT", zlib.compress(raw, 9)))
    png.extend(chunk(b"IEND", b""))
    with open(path, "wb") as handle:
        handle.write(png)


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    for filename in os.listdir(OUT_DIR):
        if filename.startswith("cursor_motion_") and filename.endswith(".png"):
            os.remove(os.path.join(OUT_DIR, filename))

    atlas = bytearray()
    for distance in range(MAX_DISTANCE + 1):
        channels = sprite_channels(distance)
        for blue_alpha, white_alpha in channels:
            atlas.extend((blue_alpha, white_alpha))
        write_png(os.path.join(OUT_DIR, f"cursor_motion_{distance:03}.png"), channels)
    with open(os.path.join(OUT_DIR, "cursor_motion_alpha.bin"), "wb") as handle:
        handle.write(atlas)


if __name__ == "__main__":
    main()
