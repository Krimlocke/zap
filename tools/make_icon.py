"""Generates the tray icons: a plain lightning bolt drawn as a white outline, no fill.

  assets/zap.ico         active
  assets/zap-paused.ico  paused (same bolt, dimmed)

Each size is rendered separately from the vector shape (supersampled), so the
small tray sizes stay crisp instead of being shrunk from the 256px image.

    pip install pillow
    python tools/make_icon.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
SUPERSAMPLE = 8

# Bolt outline in a 256x256 design space.
BOLT = [(104, 8), (188, 8), (148, 98), (208, 98), (82, 250), (114, 138), (52, 138)]

ACTIVE = (255, 255, 255, 255)
PAUSED = (255, 255, 255, 110)


def render(size: int, colour: tuple) -> Image.Image:
    big = size * SUPERSAMPLE
    # Line width in final pixels: thin at large sizes, never under ~1.3px when small.
    stroke = max(size * 12 / 256, 1.3) * SUPERSAMPLE

    # Fit the bolt inside the canvas, leaving room for the line.
    xs, ys = [p[0] for p in BOLT], [p[1] for p in BOLT]
    w, h = max(xs) - min(xs), max(ys) - min(ys)
    scale = (big - stroke - SUPERSAMPLE) / max(w, h)
    ox = (big - w * scale) / 2 - min(xs) * scale
    oy = (big - h * scale) / 2 - min(ys) * scale
    pts = [(x * scale + ox, y * scale + oy) for x, y in BOLT]

    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    # Draw opaque, then apply the colour's alpha, so overlapping line joins don't
    # show up as darker spots on the dimmed icon.
    ImageDraw.Draw(img).line(pts + pts[:2], fill=colour[:3] + (255,), width=round(stroke), joint="curve")
    if colour[3] < 255:
        img.putalpha(img.getchannel("A").point(lambda a: a * colour[3] // 255))

    return img.resize((size, size), Image.LANCZOS)


def write_ico(name: str, colour: tuple) -> None:
    out = Path(__file__).resolve().parent.parent / "assets" / name
    images = [render(s, colour) for s in SIZES]
    images[-1].save(out, sizes=[(s, s) for s in SIZES], append_images=images[:-1])
    print(f"wrote {out}")


def main() -> None:
    write_ico("zap.ico", ACTIVE)
    write_ico("zap-paused.ico", PAUSED)


if __name__ == "__main__":
    main()
