"""Omni mark, "flowing ring" (Г2) — generator of the master SVGs in this folder.

Three equal arcs with round ends and no centre (no central server). Each arc's gradient runs from its
own colour into the next arc's, so the colour flows round the ring and never starts or stops.

Parameters as picked in the editor 2026-10-17: rotation 30 deg, gap 26 deg, stroke 9, ring radius 34.
Small (<= 24 px): stroke 13, gap 34 deg, flat colours — a gradient does not survive 16 px and a
thinner stroke closes the gaps. Mono: the same two shapes in currentColor (Windows tray, print).

Run: python make_logo.py   (writes the *.svg next to this file)
"""
import math
import os

OUT = os.path.dirname(os.path.abspath(__file__))
C, R = 60, 34
ROTATION = 30  # gap centres at 30 / 150 / 270; at 0 a gap sits on top and the mark reads as a power button
COLS = ['#3D7DFF', '#1FC3A0', '#8B5CF6']  # app accent, teal, violet
TILE = '#0E1621'  # OmniBgAppBrush

LARGE = dict(gap=26, width=9)
SMALL = dict(gap=34, width=13)


def _srgb_to_lin(c):
    return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4


def _lin_to_srgb(c):
    c = min(max(c, 0.0), 1.0)
    return 12.92 * c if c <= 0.0031308 else 1.055 * c ** (1 / 2.4) - 0.055


def _to_oklch(hexc):
    r, g, b = (_srgb_to_lin(int(hexc[i:i + 2], 16) / 255) for i in (1, 3, 5))
    l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b) ** (1 / 3)
    m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b) ** (1 / 3)
    s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b) ** (1 / 3)
    L = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s
    A = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s
    B = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s
    return L, math.hypot(A, B), math.degrees(math.atan2(B, A)) % 360


def _from_oklch(L, Ch, h):
    A, B = Ch * math.cos(math.radians(h)), Ch * math.sin(math.radians(h))
    l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3
    m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3
    s = (L - 0.0894841775 * A - 1.2914855480 * B) ** 3
    rgb = (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
           -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
           -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s)
    return '#' + ''.join(f'{round(_lin_to_srgb(c) * 255):02X}' for c in rgb)


def ramp(c0, c1, n=5):
    """Stops from c0 to c1 interpolated in OKLCH along the shorter hue path. A plain two-stop sRGB
    gradient between far-apart hues greys out in the middle (teal -> violet went dull blue-grey);
    OKLab interpolation stays dusty lilac.

    The catch: violet sits close to blue in hue, so teal -> violet can only stay saturated by passing
    THROUGH blue, and the ring turns mostly blue. On that arc the stops are spaced unevenly: teal holds,
    blue goes by fast, violet gets the last third."""
    L0, C0, h0 = _to_oklch(c0)
    L1, C1, h1 = _to_oklch(c1)
    dh = (h1 - h0 + 180) % 360 - 180
    offsets = [0, 0.40, 0.55, 0.68, 1] if abs(dh) > 100 else [k / (n - 1) for k in range(n)]
    return [(offsets[k], _from_oklch(L0 + (L1 - L0) * k / (n - 1), C0 + (C1 - C0) * k / (n - 1),
                                     h0 + dh * k / (n - 1))) for k in range(n)]


def P(a):
    t = math.radians(a)
    return C + R * math.sin(t), C - R * math.cos(t)


def f(v):
    return f'{v:.2f}'


def mark(mode, small=False, tile=None):
    p = SMALL if small else LARGE
    defs, shapes = [], []
    for i in range(3):
        c = ROTATION + 120 * i
        (x0, y0), (x1, y1) = P(c + p['gap'] / 2), P(c + 120 - p['gap'] / 2)
        if mode == 'grad':
            defs.append(f'<linearGradient id="g{i}" gradientUnits="userSpaceOnUse" x1="{f(x0)}" y1="{f(y0)}" '
                        f'x2="{f(x1)}" y2="{f(y1)}">'
                        + ''.join(f'<stop offset="{o:.2f}" stop-color="{c}"/>' for o, c in ramp(COLS[i], COLS[(i + 1) % 3]))
                        + '</linearGradient>')
            paint = f'url(#g{i})'
        elif mode == 'flat':
            paint = COLS[i]
        else:
            paint = 'currentColor'
        shapes.append(f'  <path d="M{f(x0)} {f(y0)} A{R} {R} 0 0 1 {f(x1)} {f(y1)}" fill="none" '
                      f'stroke="{paint}" stroke-width="{p["width"]}" stroke-linecap="round"/>')
    vb = '0 0 120 120' if tile else '18 18 84 84'
    back = f'  <rect width="120" height="120" rx="26" fill="{tile}"/>\n' if tile else ''
    d = f'  <defs>{"".join(defs)}</defs>\n' if defs else ''
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vb}" role="img" aria-label="Omni">\n'
            f'  <title>Omni</title>\n{d}{back}' + '\n'.join(shapes) + '\n</svg>\n')


FILES = {
    'omni-mark-gradient.svg': mark('grad'),
    'omni-mark-gradient-tile.svg': mark('grad', tile=TILE),
    'omni-mark-flat-small.svg': mark('flat', small=True),
    'omni-mark-flat-small-tile.svg': mark('flat', small=True, tile=TILE),
    'omni-mark-mono.svg': mark('mono'),
    'omni-mark-mono-small.svg': mark('mono', small=True),
}

WORDMARK_FONT = r'C:\Windows\Fonts\SegUIVar.ttf'  # Segoe UI Variable: opsz 36 = Display, wght 600 = Semibold


def wordmark_paths(text='Omni', size=60, tracking=0.01):
    """The word as outlines (no font needed to view it): Segoe UI Variable Display Semibold — its stem
    matches the ring's stroke, and it is the face the app's titles are set in. Returns (path_d, width)
    with the baseline at y=0, x from 0."""
    from fontTools.pens.svgPathPen import SVGPathPen
    from fontTools.pens.transformPen import TransformPen
    from fontTools.ttLib import TTFont

    # A glyph set at a point of the design space (not instancer.instantiateVariableFont: it fails on
    # this font's GPOS). Kerning is not applied — "Omni" has no pair that needs it.
    font = TTFont(WORDMARK_FONT)
    cmap, glyphs = font.getBestCmap(), font.getGlyphSet(location={'wght': 600, 'opsz': 36})
    scale = size / font['head'].unitsPerEm
    pen, x = SVGPathPen(glyphs), 0.0
    for ch in text:
        glyph = glyphs[cmap[ord(ch)]]
        glyph.draw(TransformPen(pen, (scale, 0, 0, -scale, x, 0)))
        x += glyph.width * scale + tracking * size
    return pen.getCommands(), x - tracking * size


def lockup(mode='grad', dark=True):
    """Horizontal lockup: the ring (gradient master) + the word. Cap height of the word ~ 0.62 of the ring."""
    ring = mark(mode)
    inner = ring.split('\n', 2)[2].rsplit('</svg>', 1)[0].replace('  <title>Omni</title>\n', '')
    d, width = wordmark_paths(size=60)
    gap, ring_size = 16, 84
    text_x, baseline = ring_size + gap, 42 + 21  # ring centre y = 42; cap height ~ 42 at size 60
    colour = '#E6EBF2' if dark else '#0E1621'
    total = text_x + width
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total:.0f} 84" role="img" aria-label="Omni">\n'
            f'  <title>Omni</title>\n'
            f'  <g transform="translate(-18 -18)">\n{inner}  </g>\n'
            f'  <path transform="translate({text_x} {baseline})" d="{d}" fill="{colour}"/>\n</svg>\n')


APP = os.path.normpath(os.path.join(OUT, '..', '..', '..', 'windows-app', 'src', 'Omni.Windows.App'))
EDGE = r'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe'


def render_png(svg_text, px, path):
    """Rasterise an SVG to a transparent PNG with headless Edge (no SVG library on this machine)."""
    import subprocess
    import tempfile
    svg = svg_text.replace('<svg ', f'<svg width="{px}" height="{px}" ', 1)
    with tempfile.TemporaryDirectory() as tmp:
        page = os.path.join(tmp, 'p.html')
        with open(page, 'w', encoding='utf-8') as fh:
            fh.write(f'<html><body style="margin:0;background:transparent">{svg}</body></html>')
        subprocess.run([EDGE, '--headless=new', '--disable-gpu', '--hide-scrollbars',
                        '--default-background-color=00000000', f'--window-size={px},{px}',
                        f'--screenshot={path}', 'file:///' + page.replace('\\', '/')],
                       check=True, capture_output=True, timeout=60)


def build_app_assets():
    """PNG set for WinUI (Assets/) and Omni.ico for the exe. Masters are rasterised once at 1024 and
    scaled down; <= 24 px comes from the small master (thicker stroke, wider gaps, flat colours)."""
    import tempfile
    from PIL import Image
    assets = os.path.join(APP, 'Assets')
    with tempfile.TemporaryDirectory() as tmp:
        big, small, mono = (os.path.join(tmp, n) for n in ('big.png', 'small.png', 'mono.png'))
        render_png(mark('grad'), 1024, big)
        render_png(mark('flat', small=True), 1024, small)
        render_png(mark('mono', small=True).replace('currentColor', '#FFFFFF'), 1024, mono)
        big, small, mono = (Image.open(x).convert('RGBA') for x in (big, small, mono))

    def sized(img, px):
        return img.resize((px, px), Image.LANCZOS)

    def master(px):
        return sized(small if px <= 24 else big, px)

    def canvas(w, h, mark_px):
        out = Image.new('RGBA', (w, h), (0, 0, 0, 0))
        out.alpha_composite(master(mark_px), ((w - mark_px) // 2, (h - mark_px) // 2))
        return out

    plan = {
        'Square44x44Logo.scale-200.png': canvas(88, 88, 88),
        'Square44x44Logo.targetsize-24_altform-unplated.png': master(24),  # also the tray icon
        'Square150x150Logo.scale-200.png': canvas(300, 300, 168),
        'Wide310x150Logo.scale-200.png': canvas(620, 300, 168),
        'SplashScreen.scale-200.png': canvas(1240, 600, 260),
        'StoreLogo.png': canvas(50, 50, 50),
        'LockScreenLogo.scale-200.png': sized(mono, 48),  # lock screen badges are white-only
    }
    for name, img in plan.items():
        img.save(os.path.join(assets, name))
    ico = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    master(256).save(os.path.join(assets, 'Omni.ico'), sizes=[(s, s) for s in ico],
                     append_images=[master(s) for s in ico[:-1]])
    return list(plan) + ['Omni.ico']


if __name__ == '__main__':
    import sys
    if os.path.exists(WORDMARK_FONT):
        FILES['omni-lockup-dark-bg.svg'] = lockup(dark=True)
        FILES['omni-lockup-light-bg.svg'] = lockup(dark=False)
    for name, body in FILES.items():
        with open(os.path.join(OUT, name), 'w', encoding='utf-8', newline='\n') as fh:
            fh.write(body)
    print('\n'.join(FILES))
    if '--assets' in sys.argv:
        print('\n'.join(build_app_assets()))
