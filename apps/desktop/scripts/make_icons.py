#!/usr/bin/env python3
"""Generate the TUIC Proxy app icon: white "T" on a blue rounded square with a
green status dot at the bottom-right.

The PNG/ICO files under apps/desktop/src-tauri/icons are committed, so this
script only needs to be run when the design changes:

    python apps/desktop/scripts/make_icons.py

Requires Pillow (development-time only; the build does not run this script).
"""

from __future__ import annotations

import argparse
import os

from PIL import Image, ImageDraw, ImageFont

BASE = 1024
BLUE_LIGHT = (79, 142, 247, 255)
BLUE = (59, 130, 246, 255)
BLUE_DARK = (29, 78, 216, 255)
DOT = (255, 255, 255, 255)

FONT_CANDIDATES = [
    r"C:\Windows\Fonts\arialbd.ttf",
    r"C:\Windows\Fonts\segoeuib.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
]


def find_font() -> str:
    for path in FONT_CANDIDATES:
        if os.path.isfile(path):
            return path
    raise SystemExit("no bold font found; add one to FONT_CANDIDATES")


def gradient(size: int) -> Image.Image:
    """Diagonal blue gradient, built from a 2x2 image and upscaled."""
    small = Image.new("RGBA", (2, 2))
    small.putpixel((0, 0), BLUE_LIGHT)
    small.putpixel((1, 0), BLUE)
    small.putpixel((0, 1), BLUE)
    small.putpixel((1, 1), BLUE_DARK)
    return small.resize((size, size), Image.Resampling.BICUBIC)


def build(size: int = BASE) -> Image.Image:
    # Rounded-square mask; the corners stay transparent.
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, size - 1, size - 1), radius=int(size * 0.22), fill=255
    )

    icon = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    icon.paste(gradient(size), (0, 0), mask)
    draw = ImageDraw.Draw(icon)

    # White "T", nudged up/left so the dot has room.
    font = ImageFont.truetype(find_font(), int(size * 0.60))
    bbox = draw.textbbox((0, 0), "T", font=font)
    tx = size * 0.465 - (bbox[2] - bbox[0]) / 2 - bbox[0]
    ty = size * 0.455 - (bbox[3] - bbox[1]) / 2 - bbox[1]
    draw.text((tx, ty), "T", font=font, fill=(255, 255, 255, 255))

    # White dot at the lower right, its bottom aligned with the T's baseline
    # (same composition as the fcp icon, with a slightly smaller dot).
    t_bottom = ty + bbox[3]
    radius = size * 0.077
    cx = size * 0.745
    cy = t_bottom - radius - size * 0.005
    draw.ellipse((cx - radius, cy - radius, cx + radius, cy + radius), fill=DOT)
    return icon


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    default_out = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "src-tauri",
        "icons",
    )
    parser.add_argument("--out", default=default_out, help="output directory")
    args = parser.parse_args()
    os.makedirs(args.out, exist_ok=True)

    master = build(BASE)
    outputs = {
        "icon.png": 512,
        "256x256.png": 256,
        "128x128.png": 128,
        "32x32.png": 32,
    }
    for name, size in outputs.items():
        image = master.resize((size, size), Image.Resampling.LANCZOS)
        path = os.path.join(args.out, name)
        image.save(path, format="PNG", optimize=True)
        print(f"wrote {path} ({size}x{size})")

    # Windows .ico: a 256px base plus the sizes Explorer/Tauri may request.
    ico_base = master.resize((256, 256), Image.Resampling.LANCZOS)
    ico_path = os.path.join(args.out, "icon.ico")
    ico_base.save(
        ico_path,
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print(f"wrote {ico_path} (multi-size)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
