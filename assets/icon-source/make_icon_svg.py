"""Builds the vp-audio-extract icon SVGs (full + small-size variant).

Same wheel language as Rusty Popper's RP icon: copper ring with glass
highlights, ember disc, chrome motif with a dark outline and orange glow,
copper subtitle. The motif here is a beamed double note over an
equalizer/waveform, i.e. "music pulled out of the table".

Usage: make_icon_svg.py <font.woff> <out_dir>
"""
import sys
from pathlib import Path

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

FONT, OUT = Path(sys.argv[1]), Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)
CX, CY = 500, 508


def text_path(text, size, cx, baseline, tracking=0.04):
    """Text outlined to one SVG path, centred on cx (no font needed to view)."""
    f = TTFont(FONT)
    gs, cmap, hmtx = f.getGlyphSet(), f.getBestCmap(), f["hmtx"]
    upm = f["head"].unitsPerEm
    k = size / upm
    names = [cmap[ord(c)] for c in text]
    adv = [hmtx[n][0] * k + tracking * size for n in names]
    width = sum(adv) - tracking * size
    x = cx - width / 2
    pen = SVGPathPen(gs)
    for n, a in zip(names, adv):
        gs[n].draw(TransformPen(pen, (k, 0, 0, -k, x, baseline)))
        x += a
    return pen.getCommands()


def note_shapes(dx=0, dy=0, s=1.0):
    """Beamed eighth-note pair as ONE path (all subpaths clockwise, nonzero fill),
    so the chrome fill has no seams where the heads, stems and beam overlap."""
    import math
    def P(x, y):
        return (CX + (x - CX) * s + dx, CY + (y - CY) * s + dy)
    def sub(pts):
        return "M" + " L".join(f"{x:.2f},{y:.2f}" for x, y in pts) + " Z"
    rx, ry, rot = 84, 62, math.radians(-24)
    paths, tops = [], {}
    for (hx, hy, top) in ((392, 628, 300), (626, 580, 252)):
        def e(t):
            return (hx + rx * math.cos(t) * math.cos(rot) - ry * math.sin(t) * math.sin(rot),
                    hy + rx * math.cos(t) * math.sin(rot) + ry * math.sin(t) * math.cos(rot))
        paths.append(sub([P(*e(2 * math.pi * i / 144)) for i in range(144)]))
        # right-most point of the rotated head: the stem's right edge sits on it
        ex, ey = e(math.atan2(-ry * math.sin(rot), rx * math.cos(rot)))
        paths.append(sub([P(ex - 40, top + 30), P(ex, top + 30), P(ex, ey), P(ex - 40, hy - 8)]))
        tops[hx] = (ex, top)
    (x1, y1), (x2, y2) = tops[392], tops[626]
    paths.append(sub([P(x1 - 40, y1 + 8), P(x2, y2 - 4), P(x2, y2 + 78), P(x1 - 40, y1 + 90)]))
    return f'<path d="{" ".join(paths)}"/>'


def eq_bars(y_mid, n, max_h, w, gap):
    """Symmetric equalizer bars centred on CX (taller towards the middle)."""
    import math
    out = []
    total = n * w + (n - 1) * gap
    x0 = CX - total / 2
    for i in range(n):
        t = (i - (n - 1) / 2) / ((n - 1) / 2)
        hgt = max_h * (0.55 + 0.45 * (math.cos(t * math.pi / 2) ** 1.4)) * (0.8 + 0.2 * math.sin(i * 2.3) ** 2)
        x = x0 + i * (w + gap)
        out.append(f'<rect x="{x:.1f}" y="{y_mid - hgt / 2:.1f}" width="{w}" height="{hgt:.1f}" rx="{w / 2}"/>')
    return "\n      ".join(out)


DEFS = """
  <defs>
    <radialGradient id="ember" cx="{cx}" cy="{ecy}" r="390" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#b8461a"/>
      <stop offset="0.35" stop-color="#7a2a12"/>
      <stop offset="0.7" stop-color="#34120b"/>
      <stop offset="1" stop-color="#14080a"/>
    </radialGradient>
    <linearGradient id="rimDark" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#3a2810"/>
      <stop offset="1" stop-color="#0c0804"/>
    </linearGradient>
    <linearGradient id="copper" x1="0.12" y1="0.1" x2="0.9" y2="0.95">
      <stop offset="0" stop-color="#d9a35c"/>
      <stop offset="0.14" stop-color="#9c6428"/>
      <stop offset="0.35" stop-color="#2e1b0b"/>
      <stop offset="0.6" stop-color="#140c06"/>
      <stop offset="0.84" stop-color="#6e4219"/>
      <stop offset="0.92" stop-color="#b07a3a"/>
      <stop offset="1" stop-color="#3a220d"/>
    </linearGradient>
    <linearGradient id="bandShade" x1="0.2" y1="0.15" x2="0.85" y2="0.9">
      <stop offset="0" stop-color="#000" stop-opacity="0"/>
      <stop offset="0.5" stop-color="#000" stop-opacity="0.5"/>
      <stop offset="1" stop-color="#000" stop-opacity="0.15"/>
    </linearGradient>
    <linearGradient id="copperLine" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#6b4319"/>
      <stop offset="0.45" stop-color="#9a6128"/>
      <stop offset="0.8" stop-color="#f0bd74"/>
      <stop offset="1" stop-color="#c98a42"/>
    </linearGradient>
    <linearGradient id="chrome" x1="0" y1="{ctop}" x2="0" y2="{cbot}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#f4f5f8"/>
      <stop offset="0.2" stop-color="#ffffff"/>
      <stop offset="0.49" stop-color="#e6e8ec"/>
      <stop offset="0.51" stop-color="{c51}"/>
      <stop offset="0.78" stop-color="#b4b8c0"/>
      <stop offset="1" stop-color="{c100}"/>
    </linearGradient>
    <linearGradient id="subCopper" x1="0" y1="{stop_}" x2="0" y2="{sbot}" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#f0c57c"/>
      <stop offset="0.3" stop-color="#e2a455"/>
      <stop offset="1" stop-color="#a9652a"/>
    </linearGradient>
    <linearGradient id="eqFill" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffb35a"/>
      <stop offset="1" stop-color="#c2501a"/>
    </linearGradient>
    <linearGradient id="glass" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.28"/>
      <stop offset="0.5" stop-color="#ffffff" stop-opacity="0.03"/>
      <stop offset="1" stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
    <filter id="glow" filterUnits="userSpaceOnUse" x="0" y="0" width="1000" height="1000"><feGaussianBlur stdDeviation="{glowblur}"/></filter>
    <filter id="soft" filterUnits="userSpaceOnUse" x="0" y="0" width="1000" height="1000"><feGaussianBlur stdDeviation="{shblur}"/></filter>
    <filter id="shadow" filterUnits="userSpaceOnUse" x="0" y="0" width="1000" height="1000"><feGaussianBlur stdDeviation="14"/></filter>
    <filter id="glint" filterUnits="userSpaceOnUse" x="0" y="0" width="1000" height="1000"><feGaussianBlur stdDeviation="3"/></filter>
    <clipPath id="disc"><circle cx="{cx}" cy="{cy}" r="363"/></clipPath>
  </defs>"""


def ring(shadow=True):
    return f"""
  <!-- wheel shadow, dark outer rim, copper band, inner copper line -->
  {'<circle cx="%d" cy="%d" r="440" fill="#000" opacity="0.55" filter="url(#shadow)"/>' % (CX + 4, CY + 6) if shadow else ''}
  <circle cx="{CX}" cy="{CY}" r="443" fill="url(#rimDark)"/>
  <circle cx="{CX}" cy="{CY}" r="412" fill="none" stroke="url(#copper)" stroke-width="44"/>
  <circle cx="{CX}" cy="{CY}" r="412" fill="none" stroke="url(#bandShade)" stroke-width="44"/>
  <circle cx="{CX}" cy="{CY}" r="434" fill="none" stroke="#000" stroke-opacity="0.45" stroke-width="4"/>
  <circle cx="{CX}" cy="{CY}" r="388" fill="none" stroke="#000" stroke-opacity="0.6" stroke-width="6"/>
  <circle cx="{CX}" cy="{CY}" r="376" fill="none" stroke="url(#copperLine)" stroke-width="14"/>
  <circle cx="{CX}" cy="{CY}" r="366" fill="#0c0604"/>"""


def crescent(a0, a1, r_out, r_in, shift):
    """Lens-shaped highlight between two arcs, tapering to points at a0/a1 (degrees)."""
    import math
    def pt(r, a, d=0.0):
        a = math.radians(a)
        return f"{CX + d * math.cos(math.radians((a0 + a1) / 2)) + r * math.cos(a):.1f} {CY + d * math.sin(math.radians((a0 + a1) / 2)) + r * math.sin(a):.1f}"
    return (f"M {pt(r_out, a0)} A {r_out} {r_out} 0 0 1 {pt(r_out, a1)} "
            f"A {r_in} {r_in} 0 0 0 {pt(r_out, a0)} Z")


def glass():
    # Highlights on top of everything, like the template's glass layer.
    return f"""
  <!-- glass: crisp crescents on the ring (left, upper-right, lower-right), haze on the disc -->
  <g filter="url(#glint)">
    <path d="{crescent(160, 250, 428, 520, 0)}" fill="#fff3d6" opacity="0.6"/>
    <path d="{crescent(-60, -22, 428, 900, 0)}" fill="#fff3d6" opacity="0.5"/>
    <path d="{crescent(35, 65, 426, 900, 0)}" fill="#ffd9a0" opacity="0.3"/>
  </g>
  <ellipse cx="{CX - 50}" cy="{CY - 215}" rx="300" ry="150" fill="url(#glass)" clip-path="url(#disc)"/>"""


def build(small):
    if small:
        s, dy = 1.10, 50          # bigger note, no subtitle, bolder outline
        stroke, glowblur, shblur = 26, 30, 10
        sub = ""
        eq = ""
    else:
        s, dy = 0.80, -48
        stroke, glowblur, shblur = 18, 24, 10
        eq = eq_bars(CY + 160, 9, 72, 22, 12)
        sub = text_path("AUDIO EXTRACT", 58, CX, CY + 245)
    notes = note_shapes(0, dy, s)
    ctop, cbot = CY + (240 - CY) * s + dy, CY + (690 - CY) * s + dy
    defs = DEFS.format(cx=CX, cy=CY, ecy=CY - 20, ctop=ctop, cbot=cbot, stop_=CY + 197, sbot=CY + 247,
                       c51="#b9bdc5" if small else "#8f949d", c100="#a4a9b2" if small else "#7d828c",
                       glowblur=glowblur, shblur=shblur)
    sub_svg = "" if small else f"""
  <!-- subtitle (outlined Barlow Semi Condensed SemiBold) -->
  <path d="{sub}" fill="#000" opacity="0.7" transform="translate(6 6)" filter="url(#soft)"/>
  <path d="{sub}" fill="#160a04" stroke="#160a04" stroke-width="8" stroke-linejoin="round"/>
  <path d="{sub}" fill="url(#subCopper)"/>"""
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="{'45 53 910 910' if small else '0 0 1000 1000'}" width="1000" height="1000">
  <title>VP Audio Extract</title>{defs}
{ring(shadow=not small)}
  <!-- ember disc -->
  <circle cx="{CX}" cy="{CY}" r="363" fill="url(#ember)"/>
  <!-- equalizer behind the note -->
  <g clip-path="url(#disc)">
    <g fill="#160a04" stroke="#160a04" stroke-width="10" stroke-linejoin="round">
      {eq}
    </g>
    <g fill="url(#eqFill)" opacity="0.95">
      {eq}
    </g>
  </g>
  <!-- note: orange glow, drop shadow, dark outline, chrome fill -->
  <g clip-path="url(#disc)">
    <g fill="#ff6e19" stroke="#ff6e19" stroke-width="{stroke + 22}" stroke-linejoin="round" opacity="{1.0 if small else 0.9}" filter="url(#glow)">{notes}</g>
    <g fill="#000" stroke="#000" stroke-width="{stroke}" stroke-linejoin="round" opacity="0.65" transform="translate(10 10)" filter="url(#soft)">{notes}</g>
  </g>
  <g fill="#160a04" stroke="#160a04" stroke-width="{stroke * 2}" stroke-linejoin="round">{notes}</g>
  <g fill="url(#chrome)">{notes}</g>{sub_svg}
{glass()}
</svg>
"""


(OUT / "vp-audio-extract.svg").write_text(build(False))
(OUT / "vp-audio-extract-small.svg").write_text(build(True))
print("ok")
