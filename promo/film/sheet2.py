"""python3 sheet2.py OUT W files... — contact sheet, 3 columns, tiles W wide"""
import sys
from PIL import Image, ImageDraw
out, w = sys.argv[1], int(sys.argv[2]); fs = sys.argv[3:]; cols = 3; h = w * 9 // 16
rows = (len(fs) + cols - 1) // cols
S = Image.new('RGB', (cols * w, rows * (h + 22)), (60, 20, 60)); d = ImageDraw.Draw(S)
for i, f in enumerate(fs):
    im = Image.open(f).convert('RGB').resize((w, h), Image.LANCZOS); x, y = (i % cols) * w, (i // cols) * (h + 22)
    S.paste(im, (x, y)); d.text((x + 6, y + h + 4), f.split('/')[-1], fill=(240, 240, 240))
S.save(out)
