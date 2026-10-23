import sys, glob
from PIL import Image, ImageDraw
fs = sys.argv[2:]; out = sys.argv[1]
cols = 4 if len(fs) > 6 else 3; w, h = 480, 270
rows = (len(fs) + cols - 1) // cols
S = Image.new('RGB', (cols * w, rows * (h + 22)), (40, 40, 40)); d = ImageDraw.Draw(S)
for i, f in enumerate(fs):
    im = Image.open(f).convert('RGB').resize((w, h), Image.LANCZOS)
    x, y = (i % cols) * w, (i // cols) * (h + 22); S.paste(im, (x, y)); d.text((x + 6, y + h + 4), f.split('/')[-1], fill=(230, 230, 230))
S.save(out)
