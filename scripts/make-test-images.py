#!/usr/bin/env python3
"""Generate a small synthetic photo library for smoke tests:
    python3 scripts/make-test-images.py DIR [COUNT] [--heic]
Needs Pillow (pip install pillow). Creates landscape and portrait JPEGs plus one exact duplicate and
one corrupt file, so the smoke test also exercises duplicate suppression and error handling.
With --heic it also writes two HEIC files (needs: pip install pillow-heif)."""
import argparse
import os
import random
import shutil

from PIL import Image, ImageDraw

ap = argparse.ArgumentParser()
ap.add_argument("dir", nargs="?", default="test-images")
ap.add_argument("count", nargs="?", type=int, default=12)
ap.add_argument("--heic", action="store_true", help="also write two .heic files")
args = ap.parse_args()
out, count = args.dir, args.count
rng = random.Random(42)
os.makedirs(os.path.join(out, "sub"), exist_ok=True)

for i in range(count):
    w, h = (1600, 1000) if i % 4 else (1000, 1600)   # every 4th image is portrait
    im = Image.new("RGB", (w, h), tuple(rng.randrange(256) for _ in range(3)))
    d = ImageDraw.Draw(im)
    for _ in range(10):
        x, y = rng.randrange(w), rng.randrange(h)
        d.ellipse([x, y, x + rng.randrange(40, 400), y + rng.randrange(40, 400)],
                  fill=tuple(rng.randrange(256) for _ in range(3)))
    d.text((20, 20), f"#{i}", fill=(255, 255, 255))
    im.save(os.path.join(out, "sub" if i % 3 == 0 else "", f"img{i:03}.jpg"), quality=85)

shutil.copy(os.path.join(out, "img001.jpg"), os.path.join(out, "sub", "duplicate-of-img001.jpg"))
with open(os.path.join(out, "broken.jpg"), "wb") as f:
    f.write(b"\xff\xd8\xff\xe0not really a jpeg")
extra = ""
if args.heic:
    from pillow_heif import register_heif_opener
    register_heif_opener()
    for name, size in (("heic-landscape.heic", (1600, 1000)), ("heic-portrait.heic", (1000, 1600))):
        Image.new("RGB", size, tuple(rng.randrange(256) for _ in range(3))).save(os.path.join(out, name))
    extra = " + 2 HEIC"
print(f"wrote {count} images{extra} + 1 duplicate + 1 corrupt file to {out}")
