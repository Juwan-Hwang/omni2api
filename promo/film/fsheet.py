"""python3 fsheet.py OUT COLS W t0 t1 step lang — contact sheet from rendered frames"""
import sys
from PIL import Image, ImageDraw
out, cols, w, t0, t1, st, L = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), float(sys.argv[4]), float(sys.argv[5]), float(sys.argv[6]), sys.argv[7]
ts = []; t = t0
while t <= t1 + 1e-6: ts.append(t); t += st
h = w * 9 // 16; rows = (len(ts) + cols - 1) // cols
S = Image.new('RGB', (cols * w, rows * (h + 18)), (60, 20, 60)); d = ImageDraw.Draw(S)
for i, t in enumerate(ts):
    f = f'film/out/frames_{L}/{int(round(t * 30)):05d}.jpg'
    im = Image.open(f).resize((w, h), Image.LANCZOS); x, y = (i % cols) * w, (i // cols) * (h + 18)
    S.paste(im, (x, y)); d.text((x + 4, y + h + 3), f'{L} {t:.2f}s', fill=(240, 240, 240))
S.save(out)
