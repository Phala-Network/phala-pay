#!/usr/bin/env python3
"""Prove every deny/clear/hold rule is protected by behavioral truth-table tests.

Runs in the checkout, restores exactly the saved source in finally, and leaves no mutants.
Do not edit the target file concurrently. Set the usual CI database URLs for the full suite.
"""
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[2]
source = root / "crates/topup/src/sanctions.rs"
original = source.read_text()
command = ["cargo", "test", "--locked", "-p", "topup", "--lib",
           "sanctions::tests", "--", "--nocapture"]
mutations = [
    ("no active snapshot holds", "_ => (false, false),", "_ => (false, true),"),
    ("verification age cannot exceed configured staleness", "age <= limit", "age >= chrono::Duration::zero()"),
    ("freshness boundary is inclusive", "age <= limit", "age < limit"),
    ("future verification timestamps hold", "age >= chrono::Duration::zero()", "true"),
    ("invalid snapshot hash cannot clear", "evidence.sha256.is_some() && fresh_at", "fresh_at"),
    ("a failed manual list read cannot clear", "snapshot.is_ok() && manual.is_ok()", "snapshot.is_ok()"),
    ("snapshot hit must deny even stale or failed reads", "snapshot_hit || manual_hit", "manual_hit"),
    ("manual hit must deny even with no snapshot", "snapshot_hit || manual_hit", "snapshot_hit"),
    ("clear needs a fresh active snapshot", "snapshot_fresh && reads_ok", "reads_ok"),
    ("clear needs successful manual and snapshot reads", "snapshot_fresh && reads_ok", "snapshot_fresh"),
    ("uncertain holds", "else {\n        SanctionsVerdict::Uncertain\n    }", "else {\n        SanctionsVerdict::Clear\n    }"),
    ("fresh successful negative clears", "else if snapshot_fresh && reads_ok {\n        SanctionsVerdict::Clear", "else if snapshot_fresh && reads_ok {\n        SanctionsVerdict::Uncertain"),
]
try:
    baseline = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
    if baseline.returncode:
        raise SystemExit("baseline failed:\n" + baseline.stdout.decode())
    print("PASS list baseline", flush=True)
    for index, (name, before, after) in enumerate(mutations):
        if original.count(before) != 1:
            raise SystemExit(f"mutation anchor not unique: {name}")
        source.write_text(original.replace(before, after))
        result = subprocess.run(command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
        output = result.stdout.decode()
        if result.returncode == 0 or "test result: FAILED" not in output:
            raise SystemExit(f"mutant survived or did not compile: {name}\n{output}")
        print(f"KILLED {index + 1}: {name}", flush=True)
        source.write_text(original)
finally:
    source.write_text(original)

core = root / "crates/core/src/screening.rs"
core_original = core.read_text()
core_command = ["cargo", "test", "--locked", "-p", "topup-core", "--lib",
                "screening::tests::sanctions_verdict_truth_table", "--", "--exact"]
core_mutants = [
    ("sanctioned verdict rejects", "SanctionsVerdict::Sanctioned => return StepOutcome::Reject(RejectReason::Sanctioned),", "SanctionsVerdict::Sanctioned => {},"),
    ("uncertain verdict retries with the stable hold code", "error: RetryError::SanctionsInconclusive,", "error: RetryError::Transient,"),
]
try:
    baseline = subprocess.run(core_command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
    if baseline.returncode:
        raise SystemExit("core baseline failed:\n" + baseline.stdout.decode())
    print("PASS action baseline", flush=True)
    # Limit the retry mutation to the production function, leaving expected outcomes unchanged.
    boundary = core_original.index("#[cfg(test)]")
    production, tests = core_original[:boundary], core_original[boundary:]
    for index, (name, before, after) in enumerate(core_mutants, start=len(mutations) + 1):
        if production.count(before) != 1:
            raise SystemExit(f"core mutation anchor not unique: {name}")
        core.write_text(production.replace(before, after) + tests)
        result = subprocess.run(core_command, cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
        output = result.stdout.decode()
        if result.returncode == 0 or "sanctions_verdict_truth_table ... FAILED" not in output:
            raise SystemExit(f"core mutant survived or did not compile: {name}\n{output}")
        print(f"KILLED {index}: {name}", flush=True)
        core.write_text(core_original)
finally:
    core.write_text(core_original)
print("PASS all 14 verdict/action mutants killed; original sources restored", flush=True)
