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
    def measure(self, check, stopped=False, stop_at=None, max_seconds=3600):
        clock = [1000]
        def sleep(seconds):
            clock[0] += seconds
        with tempfile.TemporaryDirectory(prefix="upgrade-monitor-test-") as directory:
            record, stop = Path(directory) / "record.json", Path(directory) / "stop"
            if stopped:
                stop.touch()
            def probe(_):
                healthy = check(clock[0])
                if stop_at is not None and clock[0] >= stop_at:
                    stop.touch()
                return healthy
            result = module.monitor("https://example.invalid", record, stop,
                                    max_seconds=max_seconds, check=probe, now=lambda: clock[0],
                                    elapsed=lambda: clock[0], sleep=sleep)
            self.assertTrue(record.exists())
            self.assertFalse(record.with_suffix(".partial").exists())
            self.assertEqual(json.loads(record.read_text()), result)
            return result

    def test_records_first_failure_and_recovery(self):
        result = self.measure(lambda now: now < 1010 or now >= 1190, stop_at=1210)
        self.assertEqual(result["first_failed_at"], 1010)
        self.assertEqual(result["first_healthy_at"], 1190)
        self.assertEqual(result["unavailability_seconds"], 180)
        self.assertEqual(result["status"], "recovered")

    def test_records_both_reboots_and_the_total_until_stop(self):
        result = self.measure(lambda now: not (1010 <= now < 1190 or 2500 <= now < 2660),
                              stop_at=2680)
        self.assertEqual(result["outage_windows"], [
            {"first_failed_at": 1010, "first_healthy_at": 1190, "unavailability_seconds": 180},
            {"first_failed_at": 2500, "first_healthy_at": 2660, "unavailability_seconds": 160},
        ])
        self.assertEqual(result["unavailability_seconds"], 340)
        self.assertEqual(result["observed_unavailability_seconds"], 340)
        self.assertEqual(result["first_failed_at"], 1010)
        self.assertEqual(result["first_healthy_at"], 1190)
        self.assertEqual(result["status"], "recovered")
        with tempfile.TemporaryDirectory(prefix="upgrade-monitor-test-") as directory:
            record = Path(directory) / "record.json"
            record.write_text(json.dumps(result))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.summary(record)
            self.assertIn("| Total outage duration | 340 seconds |", output.getvalue())
            self.assertIn("| 1 |", output.getvalue())
            self.assertIn("| 2 |", output.getvalue())

    def test_second_open_outage_is_not_hidden_by_first_recovery(self):
        result = self.measure(lambda now: not (1010 <= now < 1190 or now >= 2500), stop_at=2590)
        self.assertEqual(len(result["outage_windows"]), 2)
        self.assertEqual(result["status"], "incomplete")
        self.assertIsNone(result["unavailability_seconds"])
        self.assertIsNone(result["outage_windows"][1]["first_healthy_at"])
        self.assertEqual(result["observed_unavailability_seconds"], 270)

    def test_timeout_keeps_open_window(self):
        result = self.measure(lambda _: False, max_seconds=60)
        self.assertEqual(result["status"], "incomplete")
        self.assertIsNone(result["unavailability_seconds"])
        self.assertIsNone(result["first_healthy_at"])
        self.assertLessEqual(result["probe_count"], 31)
        self.assertEqual(result["observed_unavailability_seconds"], 60)

    def test_timeout_after_recovery_is_an_interrupted_observation(self):
        result = self.measure(lambda now: not 1010 <= now < 1020, max_seconds=60)
        self.assertEqual(result["status"], "observation_interrupted")
        self.assertIsNone(result["unavailability_seconds"])
        self.assertEqual(result["observed_unavailability_seconds"], 10)
        self.assertEqual(result["outage_windows"][0]["unavailability_seconds"], 10)

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

    def test_summary_does_not_report_a_canceled_total_after_recovery(self):
        result = self.measure(lambda now: not 1010 <= now < 1190, stop_at=1210)
        result["status"] = "observing"
        with tempfile.TemporaryDirectory(prefix="upgrade-monitor-test-") as directory:
            record = Path(directory) / "record.json"
            record.write_text(json.dumps(result))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                module.summary(record)
            finalized = json.loads(record.read_text())
            self.assertEqual(finalized["status"], "observation_interrupted")
            self.assertIsNone(finalized["unavailability_seconds"])
            self.assertEqual(finalized["observed_unavailability_seconds"], 180)
            self.assertIn("at least 180 seconds", output.getvalue())
            self.assertNotIn("last window has no observed recovery", output.getvalue())


if __name__ == "__main__":
    unittest.main()
