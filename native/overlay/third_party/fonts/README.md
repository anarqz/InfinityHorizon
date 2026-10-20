# Fonts embedded in infinity-overlay.dll (src/fonts.rc)

| File | Font | Licence | Source |
| --- | --- | --- | --- |
| selawk.ttf, selawksb.ttf, selawkb.ttf | Selawik 1.01 (Regular, Semibold, Bold): Microsoft's open font, metric-compatible with Segoe UI | SIL Open Font License 1.1 (Selawik-LICENSE.txt) | github.com/microsoft/Selawik, release 1.01, Selawik_Release.zip |
| FluentSystemIcons-Subset.ttf | Fluent UI System Icons, Regular: only the 23 code points the overlay draws (style.h), subset with fontTools 4.55.3 (`--no-hinting --layout-features=''`) | MIT (FluentSystemIcons-LICENSE.txt) | github.com/microsoft/fluentui-system-icons, fonts/FluentSystemIcons-Regular.ttf |

Why: Proton prefixes and the Steam Deck have no Segoe fonts (SAURON, 2026-10-19: no icons, the text
in ImGui's default font), and Microsoft's Segoe files are not ours to ship. The overlay always uses
these, embedded as RCDATA, so its look is the same everywhere and the fonts are covered by the
DLL's signature.
