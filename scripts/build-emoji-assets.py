"""Build Relay's offline Unicode 17 emoji catalog and color atlas.

Inputs are pinned in assets/emoji/sources.json. Usage: python build-emoji-assets.py INPUT_DIRECTORY
The input directory contains emoji-data.json, emoji-test-17.txt and sheet-twitter-64.png.
Only Twemoji graphics are used; no Apple/Google fallback graphics are bundled.
"""
import hashlib
import json
import re
import sys
import urllib.request
from pathlib import Path

from PIL import Image
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen

source = Path(sys.argv[1])
destination = Path(__file__).resolve().parents[1] / "crates/relay-client/assets/emoji"
destination.mkdir(parents=True, exist_ok=True)
data = json.loads((source / "emoji-data.json").read_text(encoding="utf-8"))
metadata = {}
for item in data:
    metadata[item["unified"]] = (item, item, None)
    for tones, variant in item.get("skin_variations", {}).items():
        metadata[variant["unified"]] = (variant, item, tones)

categories = []
entries = []
group = ""
for line in (source / "emoji-test-17.txt").read_text(encoding="utf-8").splitlines():
    if line.startswith("# group: "):
        group = line.removeprefix("# group: ")
        categories.append(group)
    match = re.match(r"^([0-9A-F ]+)\s*;\s*(fully-qualified|component)\s*#\s*\S+\s+E[0-9.]+\s+(.+)$", line)
    if match:
        unified = "-".join(match[1].split())
        entries.append((unified, match[3], group))

assert len(entries) == 3953, "Review Unicode version or incomplete input before regenerating"
sheet = Image.open(source / "sheet-twitter-64.png").convert("RGBA")
columns, cell = 64, 50
rows = (len(entries) + columns - 1) // columns
atlas = Image.new("RGBA", (columns * cell, rows * cell))
catalog = []
for index, (unified, name, category) in enumerate(entries):
    unicode = "".join(chr(int(value, 16)) for value in unified.split("-"))
    aliases = []
    forms = []
    base = unified
    tone = ""
    if unified in metadata:
        item, parent, tones = metadata[unified]
        assert item["has_img_twitter"], f"No licensed graphic for {unified}"
        left, top = item["sheet_x"] * 66 + 1, item["sheet_y"] * 66 + 1
        graphic = sheet.crop((left, top, left + 64, top + 64))
        base = parent["unified"]
        if tones:
            tone = tones
            suffix = "_".join(f"tone{int(value, 16) - 0x1F3FA}" for value in tones.split("-"))
            aliases = [f"{alias}_{suffix}" for alias in parent["short_names"]]
        else:
            aliases = parent["short_names"].copy()
        if item.get("non_qualified"):
            forms.append("".join(chr(int(value, 16)) for value in item["non_qualified"].split("-")))
    else:
        # Four hair components are separate Unicode entries but not emoji-data rows.
        assert unified in {"1F9B0", "1F9B1", "1F9B2", "1F9B3"}, unified
        path = source / f"{unified.lower()}.png"
        if not path.exists():
            urllib.request.urlretrieve(f"https://raw.githubusercontent.com/jdecked/twemoji/v17.0.3/assets/72x72/{unified.lower()}.png", path)
        graphic = Image.open(path).convert("RGBA")
    slug = re.sub(r"[^a-z0-9]+", "_", name.lower()).strip("_")
    aliases = list(dict.fromkeys(aliases + [slug]))
    graphic = graphic.resize((48, 48), Image.Resampling.LANCZOS)
    atlas.paste(graphic, (index % columns * cell + 1, index // columns * cell + 1))
    catalog.append(dict(unicode=unicode, name=name, aliases=aliases, forms=forms,
                        category=categories.index(category), base=base, tone=tone))

atlas.save(destination / "atlas.png", optimize=True)
(destination / "catalog.json").write_text(json.dumps(dict(version="17.0", columns=columns, cell=cell,
    categories=categories, entries=catalog), ensure_ascii=False, separators=(",", ":")), encoding="utf-8")

# Empty glyphs reserve precise image widths without changing editor text/cursor indices.
# This is original Relay layout data, not third-party typeface artwork.
characters = {ord(ch) for entry in catalog for form in [entry['unicode'], *entry['forms']] for ch in form}
characters.add(0x2003)
characters.update(range(32, 127))  # Preserve custom emoji token characters in the editor.
for name, advance in [('space', 1000), ('zero', 0)]:
    builder = FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder(['.notdef', 'slot'])
    builder.setupCharacterMap({character: 'slot' for character in characters})
    builder.setupGlyf({glyph: TTGlyphPen(None).glyph() for glyph in ['.notdef', 'slot']})
    builder.setupHorizontalMetrics({glyph: (advance, 0) for glyph in ['.notdef', 'slot']})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable({'familyName': f'Relay Emoji {name}', 'styleName': 'Regular',
        'uniqueFontIdentifier': f'Relay-Emoji-{name}-1', 'fullName': f'Relay Emoji {name}', 'psName': f'RelayEmoji{name}'})
    builder.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
    builder.setupPost()
    builder.setupMaxp()
    builder.font['head'].created = builder.font['head'].modified = 2082844800
    builder.font.recalcTimestamp = False
    builder.save(destination / f'{name}.ttf')
sources = {
    "metadata": "https://raw.githubusercontent.com/iamcal/emoji-data/13ee711e222ea17fe537bfea953c687866f16411/emoji.json",
    "graphics": "https://raw.githubusercontent.com/iamcal/emoji-data/13ee711e222ea17fe537bfea953c687866f16411/sheet_twitter_64.png",
    "unicode": "https://www.unicode.org/Public/17.0.0/emoji/emoji-test.txt",
    "components": "https://github.com/jdecked/twemoji/tree/v17.0.3/assets/72x72",
    "inputs_sha256": {file.name: hashlib.sha256(file.read_bytes()).hexdigest() for file in source.iterdir()
                       if file.suffix in {".json", ".png"} or file.name == "emoji-test-17.txt"},
    "output_sha256": {name: hashlib.sha256((destination / name).read_bytes()).hexdigest()
                       for name in ["atlas.png", "catalog.json", "space.ttf", "zero.ttf"]},
    "entries": len(catalog), "graphic_changes": "Downsampled to 48 pixels and repacked with transparent borders.",
}
(destination / "sources.json").write_text(json.dumps(sources, indent=2) + "\n", encoding="utf-8")
print(f"Bundled {len(catalog)} emoji, {len(categories)} categories, atlas {atlas.size}")
