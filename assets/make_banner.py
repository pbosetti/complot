#!/usr/bin/env python3
"""Generates assets/banner.svg, the README banner.

The plots in it are drawn from real data (a Lorenz trajectory, Gaussian
samples, noisy waves), in complot's own colours. Run it from anywhere:

    python3 assets/make_banner.py
"""

import math
import random
from pathlib import Path

W, H = 1280, 320
OUT = Path(__file__).with_name("banner.svg")

# complot's Tableau 10 palette and dark panel colours.
BLUE, ORANGE, RED, TEAL, GREEN, YELLOW = (
  "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948")
INK, MUTED, FRAME, PANEL = "#f2f2f2", "#9aa3ad", "#2c313a", "#15181d"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace"

rng = random.Random(7)


def path(points):
  """An SVG path through `points`, with coordinates rounded to 0.1 px."""
  return "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in points)


def panel(x, y, w, h, title):
  """A small complot window: frame, title bar, and its plot area (x0, y0, x1, y1)."""
  svg = [
    f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="9" fill="{PANEL}" stroke="{FRAME}"/>',
    f'<path d="M{x},{y + 24}H{x + w}" stroke="{FRAME}"/>',
    f'<text x="{x + 12}" y="{y + 16}" font-size="11" fill="{MUTED}">complot — {title}</text>',
  ]
  return svg, (x + 14, y + 38, x + w - 14, y + h - 16)


def grid(area, nx, ny):
  x0, y0, x1, y1 = area
  lines = [f'M{x0 + (x1 - x0) * i / nx:.1f},{y0}V{y1}' for i in range(nx + 1)]
  lines += [f'M{x0},{y0 + (y1 - y0) * j / ny:.1f}H{x1}' for j in range(ny + 1)]
  return f'<path d="{" ".join(lines)}" stroke="#ffffff" stroke-opacity="0.06"/>'


def lines_panel(x, y, w, h):
  svg, area = panel(x, y, w, h, "live")
  x0, y0, x1, y1 = area
  svg.append(grid(area, 4, 4))
  n = 90
  for k, (color, amp, freq, phase) in enumerate(
    [(BLUE, 0.32, 1.0, 0.0), (ORANGE, 0.22, 1.7, 1.2), (RED, 0.15, 2.6, 2.4)]):
    centre = 0.30 + 0.20 * k
    pts = []
    for i in range(n):
      t = i / (n - 1)
      v = centre + amp * math.sin(2 * math.pi * freq * t + phase) * 0.5 + rng.gauss(0, 0.008)
      pts.append((x0 + (x1 - x0) * t, y1 - (y1 - y0) * v))
    # Older samples fade out to the left, as in a stream.
    svg.append(f'<path d="{path(pts)}" fill="none" stroke="url(#fade-{k})" stroke-width="1.8"/>')
    hx, hy = pts[-1]
    svg.append(f'<circle cx="{hx:.1f}" cy="{hy:.1f}" r="3.2" fill="{color}" filter="url(#glow)"/>')
  return svg


def hist_panel(x, y, w, h):
  svg, area = panel(x, y, w, h, "hist")
  x0, y0, x1, y1 = area
  svg.append(grid(area, 4, 4))
  bins, lo, hi = 11, -3.0, 5.0
  a = [rng.gauss(0, 0.9) for _ in range(900)]
  b = [rng.gauss(2.2, 0.8) for _ in range(700)]
  count = lambda vals: [sum(lo + (hi - lo) * i / bins <= v < lo + (hi - lo) * (i + 1) / bins
                            for v in vals) for i in range(bins)]
  ca, cb = count(a), count(b)
  top = max(p + q for p, q in zip(ca, cb))
  bw = (x1 - x0) / bins
  for i, (p, q) in enumerate(zip(ca, cb)):
    bx = x0 + i * bw + bw * 0.06
    for base, n, color in [(0, p, BLUE), (p, q, ORANGE)]:
      if n == 0:
        continue
      y_top = y1 - (y1 - y0) * (base + n) / top
      height = (y1 - y0) * n / top
      svg.append(f'<rect x="{bx:.1f}" y="{y_top:.1f}" width="{bw * 0.88:.1f}" height="{height:.1f}" '
                 f'fill="{color}" fill-opacity="0.6" stroke="{color}"/>')
  return svg


def lorenz(steps=9000, dt=0.006):
  x, yv, z = 1.0, 1.0, 1.0
  out = []
  for i in range(steps):
    x, yv, z = (x + dt * 10 * (yv - x), yv + dt * (x * (28 - z) - yv), z + dt * (x * yv - 8 / 3 * z))
    if i > 200:
      out.append((x, yv, z))
  return out


def lorenz_panel(x, y, w, h):
  svg, area = panel(x, y, w, h, "lorenz")
  x0, y0, x1, y1 = area
  # complot's default 3D camera: azimuth -60°, elevation 30°.
  az, el = math.radians(-60), math.radians(30)
  d = (math.cos(el) * math.cos(az), math.cos(el) * math.sin(az), math.sin(el))
  r = (-math.sin(az), math.cos(az), 0.0)
  u = (-math.sin(el) * math.cos(az), -math.sin(el) * math.sin(az), math.cos(el))
  dot = lambda a, b: sum(p * q for p, q in zip(a, b))
  cx, cy = (x0 + x1) / 2, (y0 + y1) / 2 + 4
  scale = min(x1 - x0, y1 - y0) / (2 * math.sqrt(3)) * 1.18
  proj = lambda p: (cx + scale * dot(p, r), cy - scale * dot(p, u))

  # Back panes and their edges, as complot draws them.
  for axis in range(3):
    side = -1.0 if d[axis] > 0 else 1.0
    b, c = (axis + 1) % 3, (axis + 2) % 3
    corners = []
    for vb, vc in [(-1, -1), (1, -1), (1, 1), (-1, 1)]:
      p = [0.0, 0.0, 0.0]
      p[axis], p[b], p[c] = side, vb, vc
      corners.append(proj(p))
    svg.append(f'<path d="{path(corners)}Z" fill="#ffffff" fill-opacity="0.035" '
               f'stroke="#ffffff" stroke-opacity="0.13"/>')

  pts = lorenz()
  lim = [(min(p[i] for p in pts), max(p[i] for p in pts)) for i in range(3)]
  unit = lambda p: [2 * (p[i] - lim[i][0]) / (lim[i][1] - lim[i][0]) - 1 for i in range(3)]
  trace = [proj(unit(p)) for p in pts[::5]]
  svg.append(f'<path d="{path(trace)}" fill="none" stroke="{BLUE}" stroke-width="0.9" stroke-opacity="0.9"/>')
  for _ in range(70):
    p = [rng.gauss(0, 0.35), rng.gauss(0, 0.35), rng.gauss(-0.1, 0.3)]
    if all(-1 < v < 1 for v in p):
      px, py = proj(p)
      svg.append(f'<circle cx="{px:.1f}" cy="{py:.1f}" r="1.4" fill="{ORANGE}" fill-opacity="0.85"/>')
  return svg


def main():
  pw, ph, gap, py = 196, 236, 14, 42
  px = [612 + i * (pw + gap) for i in range(3)]

  defs = [
    '<linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">'
    '<stop offset="0" stop-color="#0f1115"/><stop offset="1" stop-color="#1b1e24"/></linearGradient>',
    '<radialGradient id="halo" cx="0.72" cy="0.5" r="0.5">'
    f'<stop offset="0" stop-color="{BLUE}" stop-opacity="0.16"/>'
    f'<stop offset="1" stop-color="{BLUE}" stop-opacity="0"/></radialGradient>',
    '<pattern id="grid" width="32" height="32" patternUnits="userSpaceOnUse">'
    '<path d="M32 0H0V32" fill="none" stroke="#ffffff" stroke-opacity="0.035"/></pattern>',
    '<filter id="glow" x="-200%" y="-200%" width="500%" height="500%">'
    '<feGaussianBlur stdDeviation="2.4" result="b"/>'
    '<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>',
  ]
  for k, color in enumerate([BLUE, ORANGE, RED]):
    defs.append(f'<linearGradient id="fade-{k}" x1="0" y1="0" x2="1" y2="0">'
                f'<stop offset="0" stop-color="{color}" stop-opacity="0.15"/>'
                f'<stop offset="1" stop-color="{color}"/></linearGradient>')

  body = [
    f'<rect width="{W}" height="{H}" fill="url(#bg)"/>',
    f'<rect width="{W}" height="{H}" fill="url(#grid)"/>',
    f'<rect width="{W}" height="{H}" fill="url(#halo)"/>',
    # Name and tagline.
    f'<text x="52" y="122" font-size="76" font-weight="700" letter-spacing="-2" fill="{INK}">'
    f'complot<tspan fill="{ORANGE}">.</tspan></text>',
    f'<text x="56" y="160" font-size="19" fill="{MUTED}">real-time plots, straight from the shell</text>',
    # The terminal.
    f'<rect x="48" y="190" width="526" height="80" rx="10" fill="#0a0c0f" stroke="{FRAME}"/>',
    *(f'<circle cx="{66 + 15 * i}" cy="206" r="4.5" fill="{c}" fill-opacity="0.85"/>'
      for i, c in enumerate([RED, YELLOW, GREEN])),
    '<text x="66" y="248" font-size="16" xml:space="preserve">'
    f'<tspan fill="{GREEN}">$</tspan>'
    '<tspan fill="#d8dee9"> tail -f sensors.csv </tspan>'
    f'<tspan fill="{YELLOW}">|</tspan>'
    f'<tspan fill="{INK}" font-weight="700"> complot</tspan>'
    f'<tspan fill="{TEAL}"> --by</tspan>'
    '<tspan fill="#d8dee9"> sensor</tspan>'
    f'<tspan fill="{TEAL}"> -c -</tspan>'
    f'<tspan fill="{INK}"> ▍<animate attributeName="fill-opacity" values="1;0;1" dur="1.1s" '
    'calcMode="discrete" repeatCount="indefinite"/></tspan></text>',
  ]
  # The stream leaves the terminal on a three-lane bus under the windows and rises
  # into each of them, as `--by` splits it; rows flow along the lanes.
  for i, color in enumerate([BLUE, ORANGE, RED]):
    lane = 290 + 9 * i
    cx = px[i] + pw / 2 + 18 * (i - 1)
    route = (f"M574,{236 + 6 * i} C592,{236 + 6 * i} 584,{lane} 604,{lane} "
             f"H{cx - 10:.0f} Q{cx:.0f},{lane} {cx:.0f},{lane - 10} V{py + ph}")
    body.append(f'<path d="{route}" fill="none" stroke="{color}" stroke-opacity="0.5" '
                'stroke-dasharray="3 4"/>')
    for k in range(2):
      # Hidden until the motion has started, so that no dot sits at the origin;
      # renderers without SMIL just show the lanes.
      body.append(f'<circle r="2.6" fill="{color}" filter="url(#glow)" visibility="hidden">'
                  '<set attributeName="visibility" to="visible" begin="0s"/>'
                  f'<animateMotion dur="2.6s" begin="-{0.4 * i + 1.3 * k:.2f}s" '
                  f'repeatCount="indefinite" path="{route}"/></circle>')
  body += lines_panel(px[0], py, pw, ph)
  body += hist_panel(px[1], py, pw, ph)
  body += lorenz_panel(px[2], py, pw, ph)

  svg = (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
         f'font-family="{MONO}" role="img" aria-label="complot: real-time plots, straight from the shell">\n'
         f'<defs>{"".join(defs)}</defs>\n' + "\n".join(body) + "\n</svg>\n")
  OUT.write_text(svg)
  print(f"wrote {OUT} ({len(svg) / 1024:.0f} kB)")


if __name__ == "__main__":
  main()
