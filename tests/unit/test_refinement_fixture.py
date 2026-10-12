# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Check physical fixture geometry without a compositor or desktop session."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


path = Path(__file__).resolve().parents[1] / "tools" / "refinement_fixture.py"
spec = importlib.util.spec_from_file_location("refinement_fixture", path)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
probe_path = path.with_name("refinement_e2e.py")
probe_spec = importlib.util.spec_from_file_location("refinement_e2e", probe_path)
probe = importlib.util.module_from_spec(probe_spec)
probe_spec.loader.exec_module(probe)


class FixtureScaleTests(unittest.TestCase):
    """Keep the producer's physical buffer and integer output scale coherent."""

    def test_physical_pixel_geometry(self) -> None:
        """A scale-two 4K output needs a 4K buffer despite its logical size."""
        self.assertEqual(fixture.buffer_geometry(1920, 1080, 2), (3840, 2160))
        self.assertEqual(fixture.buffer_geometry(1920, 1080, 1), (1920, 1080))
        self.assertEqual(fixture.buffer_geometry(960, 540, 2), (1920, 1080))

    def test_invalid_geometry_is_refused_before_allocation(self) -> None:
        """Fractional, zero, negative, and over-budget buffers cannot be guessed."""
        for values in ((0, 1, 1), (1, -1, 1), (1, 1, 0), (1, 1, 1.5),
                       (1, 1, True), (8192, 8192, 1), (8192, 1, 2)):
            with self.subTest(values=values), self.assertRaises(ValueError):
                fixture.buffer_geometry(*values)

    def test_scale_changes_select_only_the_target_output(self) -> None:
        """Only a changed scale of the selected configured output repaints it."""
        target, other = {"scale": 1}, {"scale": 1}
        state = {"output": target, "configured": True, "repaint": False}
        fixture.update_output_scale(other, 2, state)
        self.assertFalse(state["repaint"])
        fixture.update_output_scale(target, 2, state)
        self.assertTrue(state["repaint"])
        state["repaint"] = False
        fixture.update_output_scale(target, 2, state)
        self.assertFalse(state["repaint"])
        fixture.update_output_scale(target, 1, state)
        self.assertTrue(state["repaint"])
        self.assertEqual(target["scale"], 1)

    def test_initial_scale_is_saved_without_painting_unconfigured_surface(self) -> None:
        """Initial output events precede the acknowledged xdg surface geometry."""
        target = {"scale": 1}
        state = {"output": None, "configured": False, "repaint": False}
        fixture.update_output_scale(target, 2, state)
        state["output"] = target
        self.assertEqual(target["scale"], 2)
        self.assertFalse(state["repaint"])
        for invalid in (0, -1, 1.5, True):
            with self.subTest(scale=invalid), self.assertRaises(ValueError):
                fixture.update_output_scale(target, invalid, state)

    def test_producer_diagnostics_keep_valid_commits_with_parse_warnings(self) -> None:
        """Malformed diagnostics cannot hide the probe's original outcome."""
        record = {"kind": "committed", "size": [3840, 2160], "logical_size": [1920, 1080],
                  "buffer_scale": 2, "seed": 711, "generation": 1}
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "fixture.log"
            log.write_text(json.dumps(record) + '\n{invalid\n{"kind":"frame-done"}\n')
            records, warnings = probe.read_producer_commits(log)
            self.assertEqual(records, [record])
            self.assertEqual(len(warnings), 1)
            self.assertIn("Line 2", warnings[0])
            missing, warnings = probe.read_producer_commits(log.with_name("missing.log"))
            self.assertEqual(missing, [])
            self.assertEqual(len(warnings), 1)

    def test_producer_diagnostics_are_bounded(self) -> None:
        """A corrupt extra commit is reported without retaining an unbounded log."""
        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / "fixture.log"
            log.write_text('{"kind":"committed"}\n' * 33)
            records, warnings = probe.read_producer_commits(log)
            self.assertEqual(len(records), 32)
            self.assertEqual(len(warnings), 1)


if __name__ == "__main__":
    unittest.main()
