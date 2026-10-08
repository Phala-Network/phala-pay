#!/usr/bin/env python3
"""Prove refund reservations, attachment caps, hint admission and cadence regressions are detected.

Requires the normal CI database URLs and test-support fixtures. Each mutant is restored in
finally; do not edit these source files concurrently with this script.
"""
import argparse
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[2]
refunds = root / "crates/topup/src/refunds.rs"
api = root / "crates/topup/src/api/mod.rs"
repository = root / "crates/topup/src/api/repository.rs"
originals = {path: path.read_text() for path in [refunds, api, repository]}
cases = [
    (repository, "refund concurrent pending cap", "if pending >= MAX_ATTACHED_PENDING_REFUNDS {", "if false {",
     ["--test", "refunds", "refund_attachment_pending_cap_keeps_existing_checks_and_reservations"]),
    (repository, "refund rolling daily cap", "if recent >= MAX_REFUND_ATTACHMENTS_PER_DAY {", "if false {",
     ["--test", "refunds", "refund_attachment_daily_cap_is_atomic_across_merchants_and_keeps_reservations"]),
    (refunds, "refund reservation", "\n            if overdue {", "\n            if overdue {\n                sqlx::query(\"UPDATE refunds SET status='failed', failure_reason='transaction_not_found' WHERE id=$1\").bind(check.refund_id).execute(&self.pool).await?;",
     ["--test", "refunds", "an_unseen_refund_alerts_after_a_day_and_cannot_be_refunded_twice"]),
    (refunds, "refund timeout alert", "\n            if overdue {", "\n            if false {",
     ["--test", "refunds", "an_unseen_refund_alerts_after_a_day_and_cannot_be_refunded_twice"]),
    (api, "hint body before admission", ".layer(middleware::from_fn(transactions::read_body))", "",
     ["--lib", "slow_hint_bodies_are_bounded_by_global_admission"]),
    (api, "silent hint overload", "error::ApiError::database_busy().into_response()", "StatusCode::ACCEPTED.into_response()",
     ["--lib", "slow_hint_bodies_are_bounded_by_global_admission"]),
    (refunds, "refund ten-minute backoff", "WHEN $3 < refund.paid_at + interval '24 hours' THEN 600", "WHEN $3 < refund.paid_at + interval '24 hours' THEN 60",
     ["--test", "refunds", "pending_refund_cadence_uses_attachment_age_without_catching_up"]),
    (refunds, "refund hourly backoff", "ELSE 3600", "ELSE 600",
     ["--test", "refunds", "pending_refund_cadence_uses_attachment_age_without_catching_up"]),
]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--hint-only", action="store_true", help="Verify only hint admission mutations")
parser.add_argument("--caps-only", action="store_true", help="Verify only refund attachment cap mutations")
args = parser.parse_args()
if args.hint_only:
    cases = [case for case in cases if case[0] == api]
if args.caps_only:
    cases = [case for case in cases if case[0] == repository]
baselines = set()
for path, name, before, after, args in cases:
    command = ["cargo", "test", "--locked", "-p", "topup", "--all-features", *args, "--", "--nocapture"]
    try:
        if tuple(command) not in baselines:
            baseline = subprocess.run(command, cwd=root, capture_output=True, timeout=600)
            if baseline.returncode or b"1 passed" not in baseline.stdout:
                raise SystemExit(f"baseline failed or ran no test: {name}\n" + baseline.stdout.decode() + baseline.stderr.decode())
            baselines.add(tuple(command))
        original = originals[path]
        if original.count(before) != 1:
            raise SystemExit(f"mutation anchor not unique: {name}")
        mutated = original.replace(before, after)
        if name == "hint body before admission":
            anchor = ".layer(Extension(Arc::new(docs.clone())))"
            mutated = mutated.replace(anchor, ".layer(middleware::from_fn(transactions::read_body))\n        " + anchor)
        path.write_text(mutated)
        result = subprocess.run(command, cwd=root, capture_output=True, timeout=600)
        output = result.stdout.decode() + result.stderr.decode()
        if result.returncode == 0 or "test result: FAILED" not in output:
            raise SystemExit(f"mutant survived or did not compile: {name}\n{output}")
        print(f"KILLED: {name}", flush=True)
    finally:
        path.write_text(originals[path])
print(f"PASS: all {len(cases)} behavioral mutants killed; original source restored", flush=True)
