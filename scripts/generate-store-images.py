#!/usr/bin/env python3
"""Export Partner Center listing images from the original icon (requires Pillow)."""

from pathlib import Path

from PIL import Image, ImageOps


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "crates/pigtail/packaging/windows/store-listing"
BACKGROUND = (20, 24, 31, 255)


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    with Image.open(ROOT / "crates/pigtail/src/icon.png") as source:
        icon = source.convert("RGBA")

    images = [
        ("app-tile", (71, 71)),
        ("app-tile", (150, 150)),
        ("app-tile", (300, 300)),
        ("box-art", (1080, 1080)),
        ("box-art", (2160, 2160)),
        ("poster-art", (720, 1080)),
        ("poster-art", (1440, 2160)),
    ]
    for kind, (width, height) in images:
        tile = kind == "app-tile"
        background = (0, 0, 0, 0) if tile else BACKGROUND
        canvas = Image.new("RGBA", (width, height), background)
        inset = 0 if tile else round(width * 0.1)
        artwork = ImageOps.contain(
            icon,
            (width - 2 * inset, height - 2 * inset),
            method=Image.Resampling.LANCZOS,
        )
        canvas.alpha_composite(
            artwork, ((width - artwork.width) // 2, (height - artwork.height) // 2)
        )
        if not tile:
            canvas = canvas.convert("RGB")
        destination = OUTPUT / f"pigtail-{kind}-{width}x{height}.png"
        canvas.save(destination, optimize=True)
        print(destination.relative_to(ROOT))


if __name__ == "__main__":
    main()
