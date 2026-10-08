#!/usr/bin/env python3
"""Prove every deny/clear/hold rule is protected by behavioral truth-table tests.

Runs in the checkout, restores exactly the saved source in finally, and leaves no mutants.
Do not edit the target file concurrently. Set the usual CI database URLs for the full suite.
"""
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[2]
source = root / "crates/topup/src/sanctions.rs"
original = source.read_text()
command = ["cargo", "test", "--locked", "-p", "topup", "--lib",
           "sanctions::tests", "--", "--nocapture"]
mutations = [
    ("no active snapshot holds", "_ => (false, false),", "_ => (false, true),"),
    ("verification age cannot exceed configured staleness", "age <= limit", "age >= chrono::Duration::zero()"),
    ("snapshot hit must deny even stale or failed reads", "snapshot_hit || manual_hit", "manual_hit"),
    ("manual hit must deny even with no snapshot", "snapshot_hit || manual_hit", "snapshot_hit"),
    ("clear needs a fresh active snapshot", "snapshot_fresh && reads_ok", "reads_ok"),
    ("clear needs successful manual and snapshot reads", "snapshot_fresh && reads_ok", "snapshot_fresh"),
    ("uncertain holds", "else {\n        SanctionsVerdict::Uncertain\n    }", "else {\n        SanctionsVerdict::Clear\n    }"),
    ("fresh successful negative clears", "else if snapshot_fresh && reads_ok {\n        SanctionsVerdict::Clear", "else if snapshot_fresh && reads_ok {\n        SanctionsVerdict::Uncertain"),
]
with tempfile.TemporaryDirectory(prefix="sanctions-mutations-", dir=root) as directory:
    logs = Path(directory)
    try:
        baseline = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if baseline.returncode:
            raise SystemExit("baseline failed:\n" + baseline.stdout.decode())
        print("PASS baseline", flush=True)
        for index, (name, before, after) in enumerate(mutations):
            if original.count(before) != 1:
                raise SystemExit(f"mutation anchor not unique: {name}")
            source.write_text(original.replace(before, after))
            result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            output = result.stdout.decode()
            if result.returncode == 0 or "test result: FAILED" not in output:
                raise SystemExit(f"mutant survived or did not compile: {name}\n{output}")
            print(f"KILLED {index + 1}: {name}", flush=True)
            source.write_text(original)
    finally:
        source.write_text(original)
print("PASS all 8 verdict mutants killed; original source restored", flush=True)
