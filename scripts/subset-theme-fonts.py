"""Subset the theme fonts in art/source/fonts to the characters the app UI can show.

Needs fontTools (pip install fonttools). Text outside the subset (names, logs) falls back to system fonts.
"""
import pathlib
import re

from fontTools import subset

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / "art" / "source" / "fonts"
OUTPUT = ROOT / "crates" / "netburrow-app" / "assets" / "fonts"
FONTS = {
    "ZCOOLKuaiLe-Regular.ttf": "kuaile.ttf",
    "fusion-pixel-12px-proportional-zh_hans.ttf": "fusion-pixel.ttf",
}
EXTRA = "…—·•×←→↗▾✓“”‘’、，。：；！？（）《》【】「」％＋－／～"


def ui_characters() -> str:
    found = {chr(code) for code in range(0x20, 0x7F)} | set(EXTRA)
    for crate in ("netburrow-app", "netburrow-core"):
        for path in (ROOT / "crates" / crate / "src").rglob("*.rs"):
            for literal in re.findall(r'"(?:[^"\\]|\\.)*"', path.read_text(encoding="utf-8")):
                found.update(ch for ch in literal if ord(ch) > 0x7F)
    return "".join(sorted(found))


def main() -> None:
    text = ui_characters()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    options = subset.Options()
    options.hinting = False
    options.desubroutinize = True
    options.layout_features = ["kern"]
    options.name_IDs = [0, 1, 2, 3, 4, 5, 6, 13, 14]
    options.notdef_outline = True
    for source, target in FONTS.items():
        font = subset.load_font(str(SOURCE / source), options)
        subsetter = subset.Subsetter(options)
        subsetter.populate(text=text)
        subsetter.subset(font)
        subset.save_font(font, str(OUTPUT / target), options)
        print(f"{target:<18} {(OUTPUT / target).stat().st_size / 1024:7.1f} KB  ({len(text)} characters)")


if __name__ == "__main__":
    main()
