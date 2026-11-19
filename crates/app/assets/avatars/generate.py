"""Writes the fixture avatars: six invented 64x64 identicon-style PNGs.

    python crates/app/assets/avatars/generate.py

Each is a mirrored 5x5 block pattern in one color on a light tint, so they
read as avatars and not as UI icons. No real pictures.
"""
import os
import random
import struct
import zlib

SIZE = 64
CELLS = 5
MARGIN = 7
HERE = os.path.dirname(os.path.abspath(__file__))

# (foreground, background)
LOOKS = [
    ((214, 80, 64), (250, 232, 226)),
    ((52, 120, 214), (226, 238, 252)),
    ((40, 150, 110), (224, 246, 236)),
    ((196, 130, 20), (252, 240, 214)),
    ((130, 84, 210), (240, 232, 252)),
    ((20, 150, 170), (222, 244, 248)),
]


def cells(seed):
    rng = random.Random(seed)
    grid = [[False] * CELLS for _ in range(CELLS)]
    for y in range(CELLS):
        for x in range((CELLS + 1) // 2):
            grid[y][x] = grid[y][CELLS - 1 - x] = rng.random() < 0.55
    return grid


def png(pixels):
    raw = b''.join(b'\x00' + bytes(row) for row in pixels)

    def chunk(kind, data):
        body = kind + data
        return struct.pack('>I', len(data)) + body + struct.pack('>I', zlib.crc32(body))

    header = struct.pack('>IIBBBBB', SIZE, SIZE, 8, 6, 0, 0, 0)
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header)
            + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


cell = (SIZE - 2 * MARGIN) // CELLS
for index, (fg, bg) in enumerate(LOOKS):
    grid = cells(index + 1)
    rows = []
    for y in range(SIZE):
        row = []
        for x in range(SIZE):
            cx, cy = (x - MARGIN) // cell, (y - MARGIN) // cell
            on = 0 <= cx < CELLS and 0 <= cy < CELLS and x >= MARGIN and y >= MARGIN and grid[cy][cx]
            row.extend([*(fg if on else bg), 255])
        rows.append(row)
    with open(os.path.join(HERE, f'{index}.png'), 'wb') as out:
        out.write(png(rows))
