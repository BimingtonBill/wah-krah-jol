"""Tests for tools/compare_shots.py (synthetic images in a temporary directory)."""

import json
import os
import subprocess
import sys
import tempfile
import unittest

from PIL import Image

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOOL = os.path.join(REPO, "tools", "compare_shots.py")


def write_image(path, size, colour):
    Image.new("RGB", size, colour).save(path)


class CompareShotsTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = self.directory.name
        self.reference_dir = os.path.join(self.root, "references")
        self.render_dir = os.path.join(self.root, "renders")
        self.out_dir = os.path.join(self.root, "out")
        os.makedirs(self.reference_dir)
        os.makedirs(self.render_dir)
        self.shots = {
            "width": 1400,
            "height": 1050,
            "shots": [
                {
                    "name": "shot-one",
                    "worldspace_id": 60,
                    "interior_cell_id": None,
                    "position": [77000.0, 77000.0, -5000.0],
                    "yaw": 90.0,
                    "pitch": 10.0,
                    "hfov": 75.0,
                    "reference": os.path.join(self.reference_dir, "shot-one.jpg"),
                    "note": "synthetic",
                    "confidence": "medium",
                },
                {
                    "name": "shot-two",
                    "worldspace_id": None,
                    "interior_cell_id": 355355,
                    "position": [0.0, 0.0, 0.0],
                    "yaw": 0.0,
                    "pitch": 0.0,
                    "hfov": 75.0,
                    "reference": os.path.join(self.reference_dir, "shot-two.jpg"),
                    "note": "synthetic",
                    "confidence": "low",
                },
                {
                    "name": "shot-missing",
                    "worldspace_id": 60,
                    "interior_cell_id": None,
                    "position": [0.0, 0.0, 0.0],
                    "yaw": 0.0,
                    "pitch": 0.0,
                    "hfov": 75.0,
                    "reference": os.path.join(self.reference_dir, "shot-missing.jpg"),
                    "note": "no render for this one",
                    "confidence": "low",
                },
            ],
        }
        self.shots_path = os.path.join(self.root, "shots.json")
        with open(self.shots_path, "w", encoding="utf-8") as handle:
            json.dump(self.shots, handle)
        # shot-one: 4:3 reference and a 16:9 render (the tool scales to a common height).
        write_image(os.path.join(self.reference_dir, "shot-one.jpg"), (1400, 1050), (200, 30, 30))
        write_image(os.path.join(self.render_dir, "shot-one.png"), (1718, 962), (30, 30, 200))
        write_image(os.path.join(self.reference_dir, "shot-two.jpg"), (800, 600), (30, 200, 30))
        write_image(os.path.join(self.render_dir, "shot-two.png"), (400, 300), (200, 200, 30))
        write_image(os.path.join(self.reference_dir, "shot-missing.jpg"), (800, 600), (60, 60, 60))

    def tearDown(self):
        self.directory.cleanup()

    def run_tool(self, *arguments):
        return subprocess.run(
            [sys.executable, TOOL, *arguments],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_writes_one_pair_per_render_and_a_contact_sheet(self):
        result = self.run_tool(self.shots_path, self.render_dir, self.out_dir, "--height", "200")
        self.assertEqual(result.returncode, 0, result.stderr)
        first = os.path.join(self.out_dir, "shot-one.jpg")
        second = os.path.join(self.out_dir, "shot-two.jpg")
        sheet = os.path.join(self.out_dir, "_sheet.jpg")
        self.assertTrue(os.path.exists(first))
        self.assertTrue(os.path.exists(second))
        self.assertTrue(os.path.exists(sheet))
        self.assertFalse(os.path.exists(os.path.join(self.out_dir, "shot-missing.jpg")))
        self.assertIn("shot-missing", result.stdout)

    def test_pair_holds_both_images_at_the_same_height(self):
        result = self.run_tool(self.shots_path, self.render_dir, self.out_dir, "--height", "200")
        self.assertEqual(result.returncode, 0, result.stderr)
        with Image.open(os.path.join(self.out_dir, "shot-one.jpg")) as image:
            # Both sources are scaled to a height of 200 and pasted side by side with a
            # 6 px divider between them.
            reference_width = round(1400 * 200 / 1050)
            render_width = round(1718 * 200 / 962)
            self.assertEqual(image.height, 200 + 52)
            self.assertEqual(image.width, reference_width + render_width + 6)

    def test_reference_falls_back_to_the_uesp_folder(self):
        # A shot whose `reference` is absent must not crash: it is reported and skipped.
        shots = json.loads(json.dumps(self.shots))
        shots["shots"][1]["reference"] = os.path.join(self.root, "not-there.jpg")
        path = os.path.join(self.root, "shots-missing-reference.json")
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(shots, handle)
        result = self.run_tool(path, self.render_dir, self.out_dir, "--height", "120")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("shot-two", result.stdout)
        self.assertFalse(os.path.exists(os.path.join(self.out_dir, "shot-two.jpg")))

    def test_contact_sheet_holds_every_pair(self):
        # The sheet used to be clamped to --sheet-height and stop mid-way: 10 of 25 pairs at the
        # default size, while the log said 25.  Every pair must be on it.
        shots = {
            "width": 1400,
            "height": 1050,
            "shots": [
                {
                    "name": f"shot-{index:02d}",
                    "worldspace_id": 60,
                    "interior_cell_id": None,
                    "position": [0.0, 0.0, 0.0],
                    "yaw": 0.0,
                    "pitch": 0.0,
                    "hfov": 75.0,
                    "reference": os.path.join(self.reference_dir, f"shot-{index:02d}.jpg"),
                    "note": "synthetic",
                    "confidence": "low",
                }
                for index in range(25)
            ],
        }
        path = os.path.join(self.root, "shots-25.json")
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(shots, handle)
        for index in range(25):
            write_image(
                os.path.join(self.reference_dir, f"shot-{index:02d}.jpg"), (200, 150), (200, 30, 30)
            )
            # A distinct colour per render, so the last row can be found on the sheet.
            write_image(
                os.path.join(self.render_dir, f"shot-{index:02d}.png"),
                (200, 150),
                (10, 10, 40 + index * 8),
            )
        result = self.run_tool(path, self.render_dir, self.out_dir, "--height", "100")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("all 25 pairs", result.stdout)
        with Image.open(os.path.join(self.out_dir, "_sheet.jpg")) as sheet:
            cell_width = (sheet.width - 3 * 8) // 2
            cell_height = int(cell_width * 0.5) + 60
            # 25 pairs at 2 columns is 13 rows: the last pair is in the bottom-left cell.
            self.assertGreaterEqual(sheet.height, 13 * cell_height + 8)
            last = sheet.getpixel((8 + cell_width // 2, 8 + 12 * cell_height + 20))
            self.assertNotEqual(last, (10, 10, 12))

    def test_no_renders_still_exits_cleanly(self):
        empty = os.path.join(self.root, "empty")
        os.makedirs(empty)
        result = self.run_tool(self.shots_path, empty, self.out_dir, "--height", "120")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("0 of 3 shots compared", result.stdout)
        self.assertFalse(os.path.exists(os.path.join(self.out_dir, "_sheet.jpg")))


if __name__ == "__main__":
    unittest.main()
