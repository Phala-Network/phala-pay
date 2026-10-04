#!/usr/bin/env python3
"""Exercise downtime recording with a fake clock and no network or CVM."""
import importlib.util
from pathlib import Path
import tempfile
import contextlib
import io
import json
import unittest

spec = importlib.util.spec_from_file_location("upgrade_monitor", Path(__file__).parents[1] / "upgrade-monitor.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class MonitorTests(unittest.TestCase):
    def measure(self, check, stopped=False):
        clock = [1000]
        def sleep(seconds):
            clock[0] += seconds
        with tempfile.TemporaryDirectory(prefix="upgrade-monitor-test-") as directory:
            record, stop = Path(directory) / "record.json", Path(directory) / "stop"
            if stopped:
                stop.touch()
            result = module.monitor("https://example.invalid", record, stop,
                                    check=lambda _: check(clock[0]), now=lambda: clock[0], sleep=sleep)
            self.assertTrue(record.exists())
            self.assertFalse(record.with_suffix(".partial").exists())
            return result

    def test_records_first_failure_and_recovery(self):
        result = self.measure(lambda now: now < 1010 or now >= 1190)
        self.assertEqual(result["first_failed_at"], 1010)
        self.assertEqual(result["first_healthy_at"], 1190)
        self.assertEqual(result["unavailability_seconds"], 180)
        self.assertEqual(result["status"], "recovered")

    def test_timeout_keeps_open_window(self):
        result = self.measure(lambda _: False)
        self.assertEqual(result["status"], "incomplete")
        self.assertIsNone(result["unavailability_seconds"])
        self.assertIsNone(result["first_healthy_at"])
        self.assertLessEqual(result["probe_count"], 451)

    def test_no_failure_is_distinct_from_zero_downtime(self):
        result = self.measure(lambda _: True, stopped=True)
        self.assertEqual(result["status"], "no_failure_observed")
        self.assertIsNone(result["first_failed_at"])

    def test_summary_finalizes_a_canceled_observer(self):
        with tempfile.TemporaryDirectory(prefix="upgrade-monitor-test-") as directory:
            record = Path(directory) / "record.json"
            record.write_text(json.dumps({"status": "observing", "first_failed_at": 1000,
                                          "first_healthy_at": None, "unavailability_seconds": None}))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.summary(record)
            self.assertEqual(json.loads(record.read_text())["status"], "incomplete")
            self.assertIn("Not measured", output.getvalue())
            self.assertIn("PST", output.getvalue())
            self.assertIn("UTC", output.getvalue())


if __name__ == "__main__":
    unittest.main()
