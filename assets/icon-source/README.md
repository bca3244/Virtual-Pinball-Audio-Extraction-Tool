# App icon source

The icon follows Rusty Popper's wheel style: copper ring, ember disc, and a chrome motif. Here the motif is a beamed note over an equalizer.

- `vp-audio-extract.svg` is the full art. The `.ico` uses it at 48 px and up.
- `vp-audio-extract-small.svg` drops the subtitle and EQ and enlarges the note. The `.ico` uses it at 40 px and below.
- The subtitle is Barlow Semi Condensed SemiBold (SIL OFL), outlined to paths. Viewing the SVGs needs no font.

To rebuild after editing `make_icon_svg.py` (Python with fontTools and Pillow, plus Node with Playwright):

```
python make_icon_svg.py barlow-semi-condensed-latin-600-normal.woff out
node render.js $PWD/out/vp-audio-extract.svg $PWD/out/full-1000.png $PWD/out/vp-audio-extract-small.svg $PWD/out/small-1000.png
python make_ico.py out/full-1000.png out/small-1000.png ../vp-audio-extract.ico ../vp-audio-extract-256.png
```
