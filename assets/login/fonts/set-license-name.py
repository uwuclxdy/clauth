#!/usr/bin/env python3
"""Write the Onest subset's license into its name table.

The OFL asks every copy of the font to carry the license. The subset ships
inside the clauth binary, so its name ID 13 (license description) carries
`OFL.txt` verbatim; the repo and the release asset carry the file too.

Run after replacing onest-latin.woff2 with a fresh upstream subset:

    python3 assets/login/fonts/set-license-name.py

Requires fontTools and brotli.
"""
from pathlib import Path

from fontTools.ttLib import TTFont

HERE = Path(__file__).resolve().parent
FONT = HERE / "onest-latin.woff2"
LICENSE = (HERE / "OFL.txt").read_text(encoding="utf-8")

font = TTFont(FONT, recalcTimestamp=False)
font["name"].setName(LICENSE, 13, 3, 1, 0x0409)
font.flavor = "woff2"
font.save(FONT)
print(f"{FONT.name}: name ID 13 carries {len(LICENSE)} characters")
