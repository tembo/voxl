"""A still, detailed picture: loadgen's text-like texture rendered once, for a window that
changes the whole screen when it appears and then stays put (stale.sh shows it with feh).
Usage: stillpic.py <out.png> <width> <height> [text|photo]
(photo: random 4x4 colour blocks, detail at every scale, like a busy web page or photo)"""
import sys
import numpy as np
from PIL import Image

out, W, H = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
kind = sys.argv[4] if len(sys.argv) > 4 else 'text'
y, x = np.mgrid[0:H, 0:W]
line, iny = y // 24, y % 24
img = np.full((H, W, 3), 245, np.uint8)
text = ((iny > 4) & (iny < 18) & (x > 40) & (x < 40 + (line * 7919) % (W - 200))
        & ((x // 9 + line * 31) % 7 != 0) & (x % 9 < 7) & ((x * 13 + y * 7 + line) % 5 < 3))
img[text] = [32, 32, 32]
img[line % 17 == 0] = [208, 112, 48]
if kind == 'photo':
    rng = np.random.default_rng(1)
    tiles = rng.integers(0, 256, (H // 4 + 1, W // 4 + 1, 3), dtype=np.uint8)
    img = np.repeat(np.repeat(tiles, 4, axis=0), 4, axis=1)[:H, :W]
Image.fromarray(img).save(out)
