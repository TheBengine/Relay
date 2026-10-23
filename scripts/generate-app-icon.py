"""Render Relay's vector mark into the committed window PNG and Windows ICO.

Pillow is needed only to regenerate assets; building Relay uses the saved files.
"""
from pathlib import Path
import xml.etree.ElementTree as ET

from PIL import Image, ImageDraw


def render(size):
    source = Path(__file__).resolve().parents[1] / "crates/relay-client/assets/relay.svg"
    scale = size * 4 / 256
    image = Image.new("RGBA", (size * 4, size * 4))
    draw = ImageDraw.Draw(image)
    for element in ET.parse(source).getroot():
        a = element.attrib
        if element.tag.endswith("rect"):
            x, y, w, h = (float(a[key]) * scale for key in ("x", "y", "width", "height"))
            draw.rounded_rectangle((x, y, x + w, y + h), radius=float(a["rx"]) * scale, fill=a["fill"])
        elif element.tag.endswith("circle"):
            x, y, r = (float(a[key]) * scale for key in ("cx", "cy", "r"))
            if a["fill"] == "none":
                half = float(a["stroke-width"]) * scale / 2
                draw.ellipse((x-r-half, y-r-half, x+r+half, y+r+half), fill=a["stroke"])
                draw.ellipse((x-r+half, y-r+half, x+r-half, y+r-half), fill="#5865f2")
            else:
                draw.ellipse((x-r, y-r, x+r, y+r), fill=a["fill"])
    return image.resize((size, size), Image.Resampling.LANCZOS)


if __name__ == "__main__":
    assets = Path(__file__).resolve().parents[1] / "crates/relay-client/assets"
    icon = render(256)
    icon.save(assets / "relay.png")
    icon.save(assets / "relay.ico", sizes=[(n, n) for n in (16, 20, 24, 32, 40, 48, 64, 128, 256)])
    print("Relay PNG and nine Windows icon sizes generated from relay.svg")
