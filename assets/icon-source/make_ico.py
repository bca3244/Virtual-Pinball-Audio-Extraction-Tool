"""Packs the rendered 1000px art into a multi-size .ico (+ a 256px PNG for the window icon).
Sizes >= 48 come from the full art, smaller ones from the simplified art."""
import sys
from PIL import Image
full, small, out_ico, out_png = sys.argv[1:5]
F, Sm = Image.open(full).convert('RGBA'), Image.open(small).convert('RGBA')
sizes = [256, 128, 64, 48, 40, 32, 24, 20, 16]
imgs = [(F if s >= 48 else Sm).resize((s, s), Image.LANCZOS) for s in sizes]
imgs[0].save(out_ico, format='ICO', sizes=[(s, s) for s in sizes], append_images=imgs[1:])
imgs[0].save(out_png)
chk = Image.open(out_ico)
print(sorted(chk.info['sizes']))
