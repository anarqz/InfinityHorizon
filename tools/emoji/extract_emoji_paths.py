#!/usr/bin/env python3
"""OD-119 (T3c): pre-extracts Noto Emoji's monochrome glyph outlines into WPF path data, so the launcher draws emoji in news text
as plain vector geometry. Nothing parses a font at runtime and no font fallback is involved (Wine's WPF does not resolve the
composite-font fallback). Run after changing the bundled font:

    python tools/emoji/extract_emoji_paths.py tools/emoji/NotoEmoji-Regular.ttf src/Infinity.App/Assets/Fonts/selawk.ttf \
        src/Infinity.App/Assets/Emoji/noto-emoji-paths.txt.gz

Code points the base font (Selawik) already has stay text (™, ↔, …); so do ASCII and Latin (keycap bases are digits).

Output (gzip, UTF-8 lines): a header "units <n> ascent <a> descent <d>", then "<hex code point>\t<advance>\t<path>", coordinates in
1/<n> em with y pointing down and 0 at the ascent line. Requires fontTools (pip install fonttools==4.55.3).
"""
import gzip
import sys

from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont

UNITS = 512  # per em: enough for emoji drawn at text sizes, a quarter of the font's 2048


def main(font_path: str, base_path: str, out_path: str) -> None:
    base = set(TTFont(base_path).getBestCmap())
    font = TTFont(font_path)
    upm = font["head"].unitsPerEm
    scale = UNITS / upm
    glyphs = font.getGlyphSet()
    hhea = font["hhea"]
    ascent = hhea.ascent * scale
    lines = [f"units {UNITS} ascent {round(ascent)} descent {round(-hhea.descent * scale)}"]
    for code_point, name in sorted(font.getBestCmap().items()):
        if code_point < 0x2000 or code_point in base:
            continue
        pen = SVGPathPen(glyphs, ntos=lambda value: str(round(value)))
        glyphs[name].draw(TransformPen(pen, (scale, 0, 0, -scale, 0, ascent)))
        path = pen.getCommands()
        if path:
            lines.append(f"{code_point:x}\t{round(glyphs[name].width * scale)}\t{path}")
    with gzip.open(out_path, "wt", encoding="utf-8", newline="\n", compresslevel=9) as out:
        out.write("\n".join(lines) + "\n")
    print(f"{len(lines) - 1} emoji -> {out_path}")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], sys.argv[3])
