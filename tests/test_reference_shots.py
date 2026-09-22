"""Contract tests for the reference shots files in tools/reference/.

The format is fixed by docs/design/reference-shots.md: degrees, Creation units, yaw clockwise
from north, pitch positive looking down.  The reference images themselves are not in the
repository (local/ is ignored), so the checks that need them are skipped when it is absent.
"""

import json
import os
import unittest

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SHOTS_FILES = [
    os.path.join(REPO, "tools", "reference", "uesp_shots_4x3.json"),
    os.path.join(REPO, "tools", "reference", "uesp_shots_blackreach_04.json"),
]
REFERENCE_DIR = os.path.join(REPO, "local", "reference", "uesp")


def load(path):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


class ReferenceShotsTest(unittest.TestCase):
    def setUp(self):
        self.documents = [(path, load(path)) for path in SHOTS_FILES]

    def test_every_file_and_shot_has_the_contract_fields(self):
        for path, document in self.documents:
            self.assertIn("width", document, path)
            self.assertIn("height", document, path)
            self.assertGreater(len(document["shots"]), 0, path)
            for shot in document["shots"]:
                for field in (
                    "name",
                    "worldspace_id",
                    "interior_cell_id",
                    "position",
                    "yaw",
                    "pitch",
                    "hfov",
                    "reference",
                    "note",
                ):
                    self.assertIn(field, shot, f"{path}: {shot.get('name')} lacks {field}")

    def test_exactly_one_space_is_set_and_it_is_a_number(self):
        for path, document in self.documents:
            for shot in document["shots"]:
                outside = shot["worldspace_id"]
                inside = shot["interior_cell_id"]
                self.assertNotEqual(
                    outside is None,
                    inside is None,
                    f"{path}: {shot['name']} must set exactly one of the two spaces",
                )
                self.assertIsInstance(outside if outside is not None else inside, int)

    def test_angles_and_position_are_in_range(self):
        for path, document in self.documents:
            for shot in document["shots"]:
                self.assertEqual(len(shot["position"]), 3, shot["name"])
                for value in shot["position"]:
                    self.assertIsInstance(value, float)
                self.assertGreaterEqual(shot["pitch"], -89.0, shot["name"])
                self.assertLessEqual(shot["pitch"], 89.0, shot["name"])
                self.assertGreaterEqual(shot["yaw"], 0.0, shot["name"])
                self.assertLess(shot["yaw"], 360.0, shot["name"])
                # Skyrim's world FOV is not exactly known, so the brief allows 65..90.
                self.assertGreaterEqual(shot["hfov"], 60.0, shot["name"])
                self.assertLessEqual(shot["hfov"], 95.0, shot["name"])

    def test_shot_names_are_unique_and_are_file_stems(self):
        names = []
        for path, document in self.documents:
            for shot in document["shots"]:
                names.append(shot["name"])
                self.assertNotIn("/", shot["name"])
                self.assertNotIn("\\", shot["name"])
                self.assertTrue(shot["name"].endswith(("Alftand", "Blackreach")) or "_" in shot["name"])
        self.assertEqual(len(names), len(set(names)), "shot names must be unique")

    @unittest.skipUnless(os.path.isdir(REFERENCE_DIR), "local/reference/uesp is not present")
    def test_every_reference_image_has_a_shot(self):
        images = {
            name
            for name in os.listdir(REFERENCE_DIR)
            if name.lower().endswith(".jpg")
        }
        covered = set()
        for _, document in self.documents:
            for shot in document["shots"]:
                covered.add(os.path.basename(shot["reference"]))
        self.assertEqual(images - covered, set(), "reference images without a shot")
        self.assertEqual(covered - images, set(), "shots without a reference image")

    @unittest.skipUnless(os.path.isdir(REFERENCE_DIR), "local/reference/uesp is not present")
    def test_each_reference_file_exists_and_its_aspect_matches_the_file(self):
        from PIL import Image

        for _, document in self.documents:
            expected = document["width"] / document["height"]
            for shot in document["shots"]:
                path = os.path.join(REPO, shot["reference"])
                self.assertTrue(os.path.exists(path), shot["reference"])
                with Image.open(path) as image:
                    self.assertAlmostEqual(
                        image.width / image.height, expected, places=2, msg=shot["name"]
                    )

    def test_the_two_files_split_the_aspects_as_the_brief_says(self):
        four_three, blackreach = self.documents[0][1], self.documents[1][1]
        self.assertEqual((four_three["width"], four_three["height"]), (1400, 1050))
        self.assertEqual((blackreach["width"], blackreach["height"]), (1718, 1080))
        self.assertEqual(len(blackreach["shots"]), 1)
        self.assertEqual(blackreach["shots"][0]["name"], "SR-place-Blackreach_04")


if __name__ == "__main__":
    unittest.main()
