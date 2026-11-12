import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

INK = (14, 23, 38)
BLUE = (47, 127, 232)
WHITE = (255, 255, 255)

SCENES = {"banner": ("banner.webp", (1000, 333), None), "home": ("banner.webp", (512, 632), (0.3, 0)), "intro": ("acropolis-banner.jpg", (900, 300), None)}
CARD = "banner.webp"


def atkinson(v, levels):
    v = v.copy()
    h, w = v.shape
    out = np.zeros((h, w), np.uint8)
    for y in range(h):
        for x in range(w):
            level = int(np.argmin(np.abs(levels - v[y, x])))
            out[y, x] = level
            error = (v[y, x] - levels[level]) / 8
            for dx, dy in ((1, 0), (2, 0), (-1, 1), (0, 1), (1, 1), (0, 2)):
                if 0 <= x + dx < w and y + dy < h:
                    v[y + dy, x + dx] += error
    return out


def tone(gray):
    v = np.asarray(gray, np.float32) / 255
    lo, hi = np.percentile(v, [2, 98])
    return np.clip((v - lo) / max(hi - lo, 0.01), 0, 1)


def scene(photo, size):
    gray = photo.convert("L").filter(ImageFilter.UnsharpMask(radius=16, percent=80, threshold=0))
    level = atkinson(tone(gray.resize(size, Image.LANCZOS)) ** 1.15, np.array([0, 0.5, 1], np.float32))
    out = Image.fromarray(np.array([INK, BLUE, WHITE], np.uint8)[level], "RGB")
    return out.resize((size[0] * 2, size[1] * 2), Image.NEAREST)


def frame(photo, size, focus):
    if focus is None:
        return photo
    x, top = focus
    photo = photo.crop((0, round(photo.height * top), photo.width, photo.height))
    width = min(photo.width, photo.height * size[0] // size[1])
    left = min(max(round(photo.width * x - width / 2), 0), photo.width - width)
    return photo.crop((left, 0, left + width, photo.height))


def main(src, out):
    src, out = Path(src), Path(out)
    for name, (file, size, focus) in SCENES.items():
        scene(frame(Image.open(src / file).convert("RGB"), size, focus), size).save(out / f"{name}.webp", lossless=True, method=6)
        print(name)
    card = Image.open(src / CARD).convert("RGB")
    width = min(card.width, card.height * 1200 // 630)
    height = width * 630 // 1200
    left, top = (card.width - width) // 2, (card.height - height) // 2
    scene(card.crop((left, top, left + width, top + height)), (600, 315)).quantize(4).save(out / "og.png", optimize=True)


main(*sys.argv[1:3])
