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


def monitor(url, record, stop, *, max_seconds=3600, check=probe, now=time.time,
            elapsed=time.monotonic, sleep=time.sleep):
    if max_seconds <= 0:
        raise ValueError("monitor duration must be positive")
    started = now()
    deadline = elapsed() + max_seconds
    data = {"started_at": started, "first_failed_at": None, "first_healthy_at": None,
            "unavailability_seconds": None, "probe_interval_seconds": 2,
            "probe_timeout_seconds": 3, "probe_count": 0, "status": "observing",
            "outage_windows": [], "observed_unavailability_seconds": 0}
    outage_started = None
    recovered_seconds = 0
    while True:
        sampled_at = now()
        sampled_elapsed = elapsed()
        try:
            healthy = check(url)
        except (OSError, subprocess.TimeoutExpired):
            healthy = False
        data["probe_count"] += 1
        if not healthy and outage_started is None:
            outage_started = sampled_elapsed
            data["outage_windows"].append({"first_failed_at": sampled_at,
                                          "first_healthy_at": None,
                                          "unavailability_seconds": None})
            if data["first_failed_at"] is None:
                data["first_failed_at"] = sampled_at
        if healthy and outage_started is not None:
            window = data["outage_windows"][-1]
            window["first_healthy_at"] = sampled_at
            window["unavailability_seconds"] = round(sampled_elapsed - outage_started, 3)
            recovered_seconds += window["unavailability_seconds"]
            outage_started = None
            if data["first_healthy_at"] is None:
                data["first_healthy_at"] = sampled_at
        data["observed_unavailability_seconds"] = round(
            recovered_seconds + (sampled_elapsed - outage_started if outage_started is not None else 0), 3)
        # Preserve the original nullable total for an open outage: completed windows alone
        # would understate the deployment. The observed total is a lower bound until recovery.
        data["unavailability_seconds"] = (
            round(recovered_seconds, 3) if data["outage_windows"] and outage_started is None else None)
        stopped = stop.exists()
        if stopped or elapsed() >= deadline:
            data["status"] = ("incomplete" if outage_started is not None else
                              "observation_interrupted" if not stopped else
                              "recovered" if data["outage_windows"] else "no_failure_observed")
            if not stopped:
                data["unavailability_seconds"] = None
        temporary = record.with_suffix(".partial")
        temporary.write_text(json.dumps(data, indent=2) + "\n")
        temporary.replace(record)
        if data["status"] != "observing":
            return data
        sleep(min(2, max(0, deadline - elapsed())))


def summary(record):
    data = json.loads(record.read_text())
    if data["status"] == "observing":
        # The runner may have canceled the monitor before its stop-file cleanup ran.
        windows = data.get("outage_windows", [])
        open_outage = (windows[-1]["first_healthy_at"] is None if windows else
                       data["first_failed_at"] is not None and data["first_healthy_at"] is None)
        data["status"] = "incomplete" if open_outage else "observation_interrupted"
        data["unavailability_seconds"] = None
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
    print(f"| Total outage duration | {str(seconds) + ' seconds' if seconds is not None else 'Not measured; see status'} |")
    if data.get("outage_windows"):
        print("\n| Outage | First failed probe | Recovery probe | Duration |\n|---|---|---|---|")
        for number, window in enumerate(data["outage_windows"], 1):
            duration = window["unavailability_seconds"]
            measured = f"{duration} seconds" if duration is not None else "Incomplete"
            print(f"| {number} | {timestamp(window['first_failed_at'])} | "
                  f"{timestamp(window['first_healthy_at'])} | {measured} |")
        if seconds is None:
            reason = ("the last window has no observed recovery" if
                      data["outage_windows"][-1]["first_healthy_at"] is None else
                      "sampling ended before the deployment stop file")
            print(f"\nObserved outage duration is at least {data['observed_unavailability_seconds']} seconds; "
                  f"{reason}.")
    print("\nProbes run every 2 seconds with a 3-second request timeout. This is sampled HTTP\n"
          "availability across all CVM restarts; maintenance admission is recorded separately.\n"
          "An incomplete window has no observed recovery. Interrupted sampling does not cover the\n"
          "whole deployment. Neither result may be reported as zero downtime or a complete total.")


if __name__ == "__main__":
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "summary":
            summary(Path(sys.argv[2]))
        elif len(sys.argv) in (4, 5):
            budget = int(sys.argv[4]) if len(sys.argv) == 5 else 3600
            monitor(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]), max_seconds=budget)
        else:
            sys.exit("usage: upgrade-monitor.py URL RECORD STOP_FILE [MAX_SECONDS] | summary RECORD")
    except (OSError, ValueError) as error:
        sys.exit(f"upgrade monitor: {error}")
