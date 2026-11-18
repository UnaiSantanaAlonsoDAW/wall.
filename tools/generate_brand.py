"""Regenerate native resources from the provided Figma SVG files.

Run with Python, Pillow and Playwright. Pass a Chromium executable if the
Playwright Chromium browser is not already installed.
"""

import argparse
import base64
from io import BytesIO
from pathlib import Path
import xml.etree.ElementTree as ET

from PIL import Image
from playwright.sync_api import sync_playwright


ROOT = Path(__file__).resolve().parents[1]
BRANDING = ROOT / "branding"
ASSETS = ROOT / "assets"
SIZES = (16, 24, 32, 48, 256)


def rasterize(page, svg, width, height):
    encoded = base64.b64encode(svg).decode("ascii")
    page.set_content(f"""
        <style>body {{ margin: 0; }} img {{ display: block; }}</style>
        <img width="{width}" height="{height}" src="data:image/svg+xml;base64,{encoded}">
    """)
    data = page.locator("img").screenshot(omit_background=True, animations="disabled")
    return Image.open(BytesIO(data)).convert("RGBA")


def icon(page, directory, prefix, name):
    frames = {}
    for size in SIZES:
        svg = (BRANDING / directory / f"{prefix}-{size}px.svg").read_bytes()
        frames[size] = rasterize(page, svg, size, size)
    frames[256].save(
        ASSETS / f"{name}.ico",
        sizes=[(size, size) for size in SIZES],
        append_images=[frames[size] for size in SIZES[:-1]],
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--chromium", type=Path, help="path to an installed Chrome or Edge")
    args = parser.parse_args()
    ASSETS.mkdir(exist_ok=True)
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=args.chromium)
        page = browser.new_page(offline=True, device_scale_factor=1)
        icon(page, "App icon Dark", "Dark", "wall")
        tile = (BRANDING / "App icon Dark" / "Dark.svg").read_bytes()
        rasterize(page, tile, 128, 128).save(ASSETS / "window-icon.png")
        icon(page, "Symbol Chalk", "Chalk", "tray-light")
        icon(page, "Symbol", "Ink", "tray-dark")

        # The supplied wordmark is outlined in Ink. Use the approved Chalk
        # palette on the dark Preferences surface, preserving its SVG outlines.
        chalk = ET.parse(BRANDING / "Symbol Chalk" / "Chalk.svg").getroot()[0].attrib["fill"]
        ink = ET.parse(BRANDING / "Symbol" / "Ink.svg").getroot()[0].attrib["fill"]
        wordmark = (BRANDING / "Wordmark" / "Ink.svg").read_bytes()
        wordmark = wordmark.replace(f'fill="{ink}"'.encode(), f'fill="{chalk}"'.encode())
        rasterize(page, wordmark, 582, 240).save(ASSETS / "wordmark.png")
        browser.close()


if __name__ == "__main__":
    main()
