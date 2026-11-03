#!/usr/bin/env python3
"""Generates the Rustle brand assets (SVG mark, wordmarks, icons) into this directory.

  python3 -m pip install fonttools
  python3 assets/brand/generate.py
  node assets/brand/render.js          # PNGs and the social preview (needs playwright)

The wordmark is converted to outlines (Inter, SIL OFL) so the SVGs look the same everywhere.
"""
import os
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen

HERE = os.path.dirname(os.path.abspath(__file__))
INTER = "/usr/share/fonts/opentype/inter/InterDisplay-Bold.otf"

GREEN_HI, GREEN_LO = "#5eead4", "#10b981"   # leaf gradient (matches the dashboard accent family)
NAVY = "#0b1020"                              # dashboard background
INK_LIGHT, INK_DARK = "#0b1020", "#f1f5f9"    # wordmark colours on light / dark backgrounds

# --- the mark, 256 x 256 ------------------------------------------------------------------------
# A single leaf tilted as if lifted by a gust, with three wind strokes trailing behind it.
LEAF = "M72 196 C58 122 112 58 206 46 C204 138 148 196 72 196 Z"
import math
_P0, _P1 = (80.0, 188.0), (186.0, 74.0)          # rib: from the base almost to the tip (never cuts the edge)
RIB = f"M{_P0[0]:.0f} {_P0[1]:.0f} L{_P1[0]:.0f} {_P1[1]:.0f}"
_dx, _dy = _P1[0] - _P0[0], _P1[1] - _P0[1]
_len = math.hypot(_dx, _dy)
_ux, _uy = _dx / _len, _dy / _len                 # unit vector along the rib
VEINS = []
for t, ln in ((.32, 40), (.55, 34), (.76, 24)):   # vein pairs sweeping toward the tip
    px, py = _P0[0] + _dx * t, _P0[1] + _dy * t
    for side in (-1, 1):
        ang = math.radians(48) * side
        vx = _ux * math.cos(ang) - _uy * math.sin(ang)
        vy = _ux * math.sin(ang) + _uy * math.cos(ang)
        VEINS.append(f"M{px:.1f} {py:.1f} L{px + vx * ln:.1f} {py + vy * ln:.1f}")
WIND = [("M16 104 C46 96 74 108 98 94", 7, 1.0), ("M30 138 C54 132 70 142 90 134", 6, .75), ("M10 72 C30 66 46 74 60 68", 5, .55)]
STEM = "M72 196 L50 222"


def mark_body(uid="a", mono=None):
    fill = mono or f"url(#g{uid})"
    wind = "".join(f'<path d="{d}" fill="none" stroke="{fill}" stroke-width="{w}" stroke-linecap="round" opacity="{o}"/>' for d, w, o in WIND)
    veins = "".join(f'<path d="{d}"/>' for d in VEINS)
    defs = "" if mono else f'<linearGradient id="g{uid}" x1="60" y1="210" x2="210" y2="40" gradientUnits="userSpaceOnUse"><stop offset="0" stop-color="{GREEN_LO}"/><stop offset="1" stop-color="{GREEN_HI}"/></linearGradient>'
    defs += f'<mask id="m{uid}" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256"><rect width="256" height="256" fill="#fff"/>' \
            f'<g fill="none" stroke="#000" stroke-linecap="round"><path d="{RIB}" stroke-width="7"/><g stroke-width="4.5">{veins}</g></g></mask>'
    return (f"<defs>{defs}</defs>{wind}"
            f'<g mask="url(#m{uid})"><path d="{LEAF}" fill="{fill}"/></g>'
            f'<path d="{STEM}" fill="none" stroke="{fill}" stroke-width="9" stroke-linecap="round"/>')


def svg(w, h, vb, body, title):
    return f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="{vb}" role="img" aria-label="{title}"><title>{title}</title>{body}</svg>\n'


def text_path(text, size, x, y):
    font = TTFont(INTER)
    gs, cmap, upm = font.getGlyphSet(), font.getBestCmap(), font["head"].unitsPerEm
    scale, cur, out = size / upm, 0, []
    hmtx = font["hmtx"]
    for ch in text:
        name = cmap[ord(ch)]
        pen = SVGPathPen(gs)
        gs[name].draw(TransformPen(pen, (scale, 0, 0, -scale, x + cur * scale, y)))
        out.append(pen.getCommands())
        cur += hmtx[name][0] - (size * 0.012 / scale)   # slightly tighter tracking
    return " ".join(out), cur * scale


def write(name, content):
    with open(os.path.join(HERE, name), "w") as f:
        f.write(content)
    print("wrote", name)


# mark: works on light and dark backgrounds
write("logo-mark.svg", svg(256, 256, "0 0 256 256", mark_body("m"), "Rustle"))
write("logo-mark-mono.svg", svg(256, 256, "0 0 256 256", mark_body("o", mono="currentColor"), "Rustle"))

# horizontal lockups (mark + wordmark outlines), light and dark backgrounds
for suffix, ink in (("", INK_LIGHT), ("-dark", INK_DARK)):
    d, width = text_path("Rustle", 150, 292, 176)
    body = f'<g transform="translate(0 0)">{mark_body("h" + suffix)}</g><path d="{d}" fill="{ink}"/>'
    write(f"logo-horizontal{suffix}.svg", svg(int(292 + width + 8), 256, f"0 0 {int(292 + width + 8)} 256", body, "Rustle"))

# app icon: mark on the dashboard's navy, rounded
icon_body = (f'<rect width="256" height="256" rx="56" fill="{NAVY}"/><g transform="translate(22 20) scale(.88)">{mark_body("i")}</g>')
write("icon.svg", svg(256, 256, "0 0 256 256", icon_body, "Rustle"))
write("favicon.svg", svg(64, 64, "0 0 256 256", icon_body, "Rustle"))
