#!/usr/bin/env python3
"""Render a `tmux capture-pane -e` ANSI dump as a crisp SVG screenshot.

Design notes (learned the hard way):
- SVG collapses runs of whitespace inside <text>, so spaces are never
  rendered as glyphs — backgrounds come from <rect> runs instead.
- One absolutely-positioned <text> per WORD, with textLength equal to the
  word's cell width. Whole-line textLength stretches glyphs (visible
  letter-spacing distortion); per-word it is invisible.

Geometry matches the checked-in docs/img/*.svg: 8.4px cells, 17px lines,
13.2px monospace, 14px page margin, rounded #101420 page.
"""

import re
import sys

CELL_W = 8.4
LINE_H = 17.0
MARGIN = 14.0
BASELINE = 12.5
FONT = "ui-monospace,'Cascadia Mono','DejaVu Sans Mono',Menlo,Consolas,monospace"
DEFAULT_FG = "#c8d0dc"
DEFAULT_BG = "#10141f"
PAGE_BG = "#101420"

BASIC = [
    "#1c2026", "#e06c75", "#98c379", "#e5c07b",
    "#61afef", "#c678dd", "#56b6c2", "#c8d0dc",
    "#5c6370", "#ff7a85", "#a9d47f", "#f0ca85",
    "#74bdf5", "#d48ae8", "#66c8d4", "#ffffff",
]


def xterm256(n):
    if n < 16:
        return BASIC[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        r, g, b = steps[n // 36], steps[(n // 6) % 6], steps[n % 6]
    else:
        v = 8 + 10 * (n - 232)
        r = g = b = v
    return f"#{r:02x}{g:02x}{b:02x}"


def blend(hex_color, toward, frac):
    a = [int(hex_color[i : i + 2], 16) for i in (1, 3, 5)]
    b = [int(toward[i : i + 2], 16) for i in (1, 3, 5)]
    mixed = [round(x + (y - x) * frac) for x, y in zip(a, b)]
    return "#{:02x}{:02x}{:02x}".format(*mixed)


class Attr:
    __slots__ = ("fg", "bg", "bold", "dim", "reverse")

    def __init__(self):
        self.fg, self.bg = DEFAULT_FG, DEFAULT_BG
        self.bold = self.dim = self.reverse = False

    def copy(self):
        a = Attr()
        a.fg, a.bg, a.bold, a.dim, a.reverse = (
            self.fg, self.bg, self.bold, self.dim, self.reverse,
        )
        return a

    def resolved(self):
        fg, bg = (self.bg, self.fg) if self.reverse else (self.fg, self.bg)
        if self.dim:
            fg = blend(fg, bg, 0.45)
        return fg, bg


def apply_sgr(attr, params):
    it = iter(params)
    for p in it:
        if p in (0, None):
            attr.__init__()
        elif p == 1:
            attr.bold = True
        elif p == 2:
            attr.dim = True
        elif p == 7:
            attr.reverse = True
        elif p in (21, 22):
            attr.bold = attr.dim = False
        elif p == 27:
            attr.reverse = False
        elif 30 <= p <= 37:
            attr.fg = BASIC[p - 30]
        elif p == 38 or p == 48:
            mode = next(it, None)
            if mode == 5:
                c = xterm256(next(it, 0) or 0)
            elif mode == 2:
                r, g, b = (next(it, 0) or 0 for _ in range(3))
                c = f"#{r:02x}{g:02x}{b:02x}"
            else:
                continue
            if p == 38:
                attr.fg = c
            else:
                attr.bg = c
        elif p == 39:
            attr.fg = DEFAULT_FG
        elif 40 <= p <= 47:
            attr.bg = BASIC[p - 40]
        elif p == 49:
            attr.bg = DEFAULT_BG
        elif 90 <= p <= 97:
            attr.fg = BASIC[p - 90 + 8]
        elif 100 <= p <= 107:
            attr.bg = BASIC[p - 100 + 8]


CSI = re.compile(r"\x1b\[([0-9;:]*)([A-Za-z])")


def parse(text, cols):
    grid = []
    attr = Attr()
    for raw in text.split("\n"):
        row = []
        i = 0
        while i < len(raw):
            ch = raw[i]
            if ch == "\x1b":
                m = CSI.match(raw, i)
                if m:
                    if m.group(2) == "m":
                        params = [
                            int(x) if x else 0
                            for x in re.split("[;:]", m.group(1))
                        ] or [0]
                        apply_sgr(attr, params)
                    i = m.end()
                    continue
                i += 1
                continue
            if ch >= " ":
                row.append((ch, attr.copy()))
            i += 1
        pad = Attr()
        while len(row) < cols:
            row.append((" ", pad))
        grid.append(row[:cols])
    while grid and all(c == " " for c, _ in grid[-1]):
        grid.pop()
    return grid


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def render(grid, cols):
    rows = len(grid)
    width = cols * CELL_W + 2 * MARGIN
    height = rows * LINE_H + 2 * MARGIN
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width:.0f}" '
        f'height="{height:.0f}" viewBox="0 0 {width:.0f} {height:.0f}">',
        f'<rect width="100%" height="100%" rx="8" fill="{PAGE_BG}"/>',
    ]
    # Background rects: one per run of identical bg per row.
    for r, row in enumerate(grid):
        y = MARGIN + r * LINE_H
        c = 0
        while c < cols:
            bg = row[c][1].resolved()[1]
            start = c
            while c < cols and row[c][1].resolved()[1] == bg:
                c += 1
            out.append(
                f'<rect x="{MARGIN + start * CELL_W:.1f}" y="{y:.1f}" '
                f'width="{(c - start) * CELL_W:.1f}" height="{LINE_H:.1f}" '
                f'fill="{bg}"/>'
            )
    out.append(f'<g font-family="{FONT}" font-size="13.2px">')
    # Text: split each row into words of identical (fg, bold).
    for r, row in enumerate(grid):
        y = MARGIN + r * LINE_H + BASELINE
        c = 0
        while c < cols:
            ch, attr = row[c]
            if ch == " ":
                c += 1
                continue
            fg, _ = attr.resolved()
            bold = attr.bold
            start = c
            word = []
            while c < cols:
                ch2, a2 = row[c]
                f2, _ = a2.resolved()
                if ch2 == " " or f2 != fg or a2.bold != bold:
                    break
                word.append(ch2)
                c += 1
            n = len(word)
            weight = ' font-weight="bold"' if bold else ""
            out.append(
                f'<text x="{MARGIN + start * CELL_W:.1f}" y="{y:.1f}" '
                f'fill="{fg}" textLength="{n * CELL_W:.1f}" '
                f'lengthAdjust="spacingAndGlyphs"{weight}>{esc("".join(word))}</text>'
            )
    out.append("</g></svg>")
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit("usage: ans2svg.py capture.ans out.svg [< cols=120 fixed >]")
    with open(sys.argv[1], encoding="utf-8", errors="replace") as f:
        grid = parse(f.read(), 120)
    with open(sys.argv[2], "w", encoding="utf-8") as f:
        f.write(render(grid, 120))
