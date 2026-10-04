#!/usr/bin/env python3
"""Bounded health sampling during upgrade; JSON stays usable on failure/cancellation."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
from datetime import datetime, timezone
from zoneinfo import ZoneInfo


def probe(url):
    result = subprocess.run(
        ["curl", "-s", "-o", os.devnull, "-w", "%{http_code}",
         "--max-time", "3", f"{url.rstrip('/')}/healthz"],
        capture_output=True, text=True, timeout=5, check=False,
    )
    return result.returncode == 0 and result.stdout == "200"


def monitor(url, record, stop, *, check=probe, now=time.time, sleep=time.sleep):
    started = now()
    data = {"started_at": started, "first_failed_at": None, "first_healthy_at": None,
            "unavailability_seconds": None, "probe_interval_seconds": 2,
            "probe_timeout_seconds": 3, "probe_count": 0, "status": "observing"}
    while True:
        sampled_at = now()
        try:
            healthy = check(url)
        except (OSError, subprocess.TimeoutExpired):
            healthy = False
        data["probe_count"] += 1
        if not healthy and data["first_failed_at"] is None:
            data["first_failed_at"] = sampled_at
        if healthy and data["first_failed_at"] is not None:
            data["first_healthy_at"] = sampled_at
            data["unavailability_seconds"] = round(sampled_at - data["first_failed_at"], 3)
            data["status"] = "recovered"
        if data["status"] != "recovered" and (stop.exists() or now() - started >= 900):
            data["status"] = "incomplete" if data["first_failed_at"] is not None else "no_failure_observed"
        temporary = record.with_suffix(".partial")
        temporary.write_text(json.dumps(data, indent=2) + "\n")
        temporary.replace(record)
        if data["status"] != "observing":
            return data
        sleep(2)


def summary(record):
    data = json.loads(record.read_text())
    if data["status"] == "observing":
        # The runner may have canceled the monitor before its stop-file cleanup ran.
        data["status"] = "incomplete" if data["first_failed_at"] is not None else "observation_interrupted"
        record.write_text(json.dumps(data, indent=2) + "\n")
    def timestamp(value):
        if value is None:
            return "Not observed"
        dt = datetime.fromtimestamp(value, timezone.utc)
        return f"{dt.astimezone(ZoneInfo('America/Los_Angeles')):%Y-%m-%d %I:%M:%S %p %Z} ({dt:%Y-%m-%d %H:%M:%S UTC})"
    print("### Observed upgrade unavailability\n")
    print("| Probe result | Value |\n|---|---|")
    print(f"| Status | {data['status']} |")
    print(f"| First failed probe | {timestamp(data['first_failed_at'])} |")
    print(f"| First healthy probe after failure | {timestamp(data['first_healthy_at'])} |")
    seconds = data['unavailability_seconds']
    print(f"| Observed window | {str(seconds) + ' seconds' if seconds is not None else 'Not measured; see status'} |")
    print("\nProbes run every 2 seconds with a 3-second request timeout. This is sampled HTTP\n"
          "availability, including the CVM restart; maintenance admission is recorded separately.\n"
          "An incomplete window has no observed recovery and must not be reported as zero downtime.")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "summary":
        summary(Path(sys.argv[2]))
    elif len(sys.argv) == 4:
        monitor(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]))
    else:
        sys.exit("usage: upgrade-monitor.py URL RECORD STOP_FILE | summary RECORD")
