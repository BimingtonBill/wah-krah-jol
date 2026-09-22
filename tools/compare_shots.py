#!/usr/bin/env python3
"""Put every reference screenshot next to the engine's render of the same camera pose.

Usage:
    python tools/compare_shots.py <shots.json> <render dir> <out dir> [options]

Options:
    --height px     height of each image in the pair (default 700)
    --sheet-width px  width of the contact sheet (default 2400)
    --sheet-height px height of the contact sheet (default 3400)

For every shot in the JSON whose render `<render dir>/<name>.png` exists, this writes
`<out dir>/<name>.jpg`: the reference on the left and the render on the right, both scaled to
the same height, with a caption bar giving the name, the pose and the confidence.  It also
writes `<out dir>/_sheet.jpg`, all pairs on one contact sheet, and prints a line per shot.

The reference path is the shot's `reference` field if it exists, otherwise
`local/reference/uesp/<name>.jpg` relative to the repository root.  Shots whose render is
missing are listed at the end and skipped, so this is safe to run while renders are still
being produced.
"""

import json
import os
import sys

from PIL import Image, ImageDraw, ImageFont

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_REFERENCE_DIR = os.path.join(REPO, "local", "reference", "uesp")
SHEET_COLUMNS = 2


def font(size):
    for name in ("segoeui.ttf", "arial.ttf", "DejaVuSans.ttf"):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            continue
    return ImageFont.load_default()


def fit_height(image, height):
    scale = height / image.height
    return image.resize((max(1, int(round(image.width * scale))), height), Image.LANCZOS)


def reference_path(shot):
    reference = shot.get("reference")
    if reference:
        candidate = reference if os.path.isabs(reference) else os.path.join(REPO, reference)
        if os.path.exists(candidate):
            return candidate
    return os.path.join(DEFAULT_REFERENCE_DIR, f"{shot['name']}.jpg")


def pose_line(shot):
    position = shot.get("position", [0.0, 0.0, 0.0])
    space = (
        f"interior {shot['interior_cell_id']}"
        if shot.get("interior_cell_id")
        else f"worldspace {shot.get('worldspace_id')}"
    )
    return (
        f"{space}  pos ({position[0]:.0f}, {position[1]:.0f}, {position[2]:.0f})  "
        f"yaw {shot.get('yaw', 0.0):.1f}  pitch {shot.get('pitch', 0.0):.1f}  "
        f"hfov {shot.get('hfov', 75.0):.1f}  [{shot.get('confidence', 'unknown')}]"
    )


def pair_image(shot, render_path, height):
    reference = fit_height(Image.open(reference_path(shot)).convert("RGB"), height)
    render = fit_height(Image.open(render_path).convert("RGB"), height)
    bar = 52
    width = reference.width + render.width + 6
    sheet = Image.new("RGB", (width, height + bar), (16, 16, 20))
    sheet.paste(reference, (0, bar))
    sheet.paste(render, (reference.width + 6, bar))
    draw = ImageDraw.Draw(sheet)
    draw.line([(reference.width + 2, bar), (reference.width + 2, height + bar)], fill=(255, 0, 0), width=2)
    draw.text((6, 4), shot["name"], fill=(255, 255, 255), font=font(20))
    draw.text((6, 28), pose_line(shot), fill=(190, 200, 220), font=font(15))
    draw.text(
        (width - 320, 28), "reference | engine", fill=(150, 220, 150), font=font(15)
    )
    return sheet


def sheet_image(pairs, width, height):
    if not pairs:
        return None
    columns = SHEET_COLUMNS
    cell_width = (width - (columns + 1) * 8) // columns
    cell_height = int(cell_width * 0.5) + 60
    rows = (len(pairs) + columns - 1) // columns
    sheet = Image.new("RGB", (width, min(height, rows * cell_height + 8)), (10, 10, 12))
    for index, pair in enumerate(pairs):
        column = index % columns
        row = index // columns
        scaled = pair.resize((cell_width, int(pair.height * cell_width / pair.width)), Image.LANCZOS)
        x = 8 + column * (cell_width + 8)
        y = 8 + row * cell_height
        if y + scaled.height > sheet.height:
            break
        sheet.paste(scaled, (x, y))
    return sheet


def main(argv):
    if len(argv) < 4:
        print(__doc__)
        return 2
    shots_file, render_dir, out_dir = argv[1], argv[2], argv[3]
    height = 700
    sheet_width, sheet_height = 2400, 3400
    index = 4
    while index < len(argv):
        if argv[index] == "--height":
            height = int(argv[index + 1])
            index += 2
        elif argv[index] == "--sheet-width":
            sheet_width = int(argv[index + 1])
            index += 2
        elif argv[index] == "--sheet-height":
            sheet_height = int(argv[index + 1])
            index += 2
        else:
            raise SystemExit(f"unknown option {argv[index]!r}")

    with open(shots_file, "r", encoding="utf-8") as handle:
        document = json.load(handle)
    os.makedirs(out_dir, exist_ok=True)
    pairs, missing, done = [], [], 0
    for shot in document.get("shots", []):
        render = os.path.join(render_dir, f"{shot['name']}.png")
        if not os.path.exists(render):
            missing.append(shot["name"])
            continue
        reference = reference_path(shot)
        if not os.path.exists(reference):
            missing.append(f"{shot['name']} (no reference at {reference})")
            continue
        pair = pair_image(shot, render, height)
        destination = os.path.join(out_dir, f"{shot['name']}.jpg")
        pair.save(destination, quality=88)
        pairs.append(pair)
        done += 1
        print(f"  {shot['name']}: {destination} {pair.width}x{pair.height}")
    if pairs:
        sheet = sheet_image(pairs, sheet_width, sheet_height)
        sheet_path = os.path.join(out_dir, "_sheet.jpg")
        sheet.save(sheet_path, quality=86)
        print(f"  contact sheet: {sheet_path} {sheet.width}x{sheet.height}, {len(pairs)} pairs")
    if missing:
        print(f"  {len(missing)} shots without a render (skipped): " + ", ".join(missing))
    print(f"  {done} of {len(document.get('shots', []))} shots compared")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
