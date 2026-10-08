#!/usr/bin/env python3
"""Prove regressions in refund safety, hint admission and finality bounds are detected.

Requires the normal CI database URLs and test-support fixtures. Each mutant is restored in
finally; do not edit these source files concurrently with this script.
"""
import argparse
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parents[2]
refunds = root / "crates/topup/src/refunds.rs"
api = root / "crates/topup/src/api/mod.rs"
repository = root / "crates/topup/src/api/repository.rs"
finality = root / "crates/topup/src/finality/mod.rs"
schedule = root / "crates/topup/migrations/20261102000000_finality_due.up.sql"
deposits = root / "crates/topup/src/db/deposits.rs"
scanner = root / "crates/topup/src/scanner/mod.rs"
originals = {path: path.read_text() for path in [refunds, api, repository, finality, schedule, deposits, scanner]}
cases = [
    (schedule, "finality ten-minute boundary", "WHEN checked_at < COALESCE(anchor, checked_at) + interval '10 minutes' THEN 60", "WHEN checked_at <= COALESCE(anchor, checked_at) + interval '10 minutes' THEN 60",
     ["--test", "finality", "unresolved_finality_cadence_keeps_first_due_time_and_exact_boundaries"]),
    (schedule, "finality six-hour boundary", "WHEN checked_at < COALESCE(anchor, checked_at) + interval '6 hours' THEN 600", "WHEN checked_at <= COALESCE(anchor, checked_at) + interval '6 hours' THEN 600",
     ["--test", "finality", "unresolved_finality_cadence_keeps_first_due_time_and_exact_boundaries"]),
    (finality, "finality replacement K bound", "if hashes.len() > 1 {", "if hashes.len() > 2 {",
     ["--test", "finality", "ambiguous_replacements_read_no_candidates_and_alert_without_reversing"]),
    (finality, "finality replacement query bound", "tx_hash != $4 LIMIT 2", "tx_hash != $4 LIMIT 1",
     ["--test", "finality", "ambiguous_replacements_read_no_candidates_and_alert_without_reversing"]),
    (repository, "refund concurrent pending cap", "if pending >= i64::from(max_attached_pending_refunds.get()) {", "if false {",
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
    (repository, "configured staging refund cap", "if pending >= i64::from(max_attached_pending_refunds.get()) {", "if pending >= 2 {",
     ["--test", "refunds", "refund_attachment_pending_cap_keeps_existing_checks_and_reservations"]),
    (schedule, "shared finality hourly backoff", "ELSE 3600", "ELSE 600",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (deposits, "confirm must use shared backoff", "next_attempt_at=finality_next_check_at(COALESCE(first_unresolved_at,$2),$2)", "next_attempt_at=$2 + interval '2 seconds'",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (deposits, "confirm unresolved persistence", "if writes.effects.first_unresolved {", "if false {",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (deposits, "confirm once-only unresolved entry", "first_unresolved_at=COALESCE(first_unresolved_at,$2)", "first_unresolved_at=$2",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (finality, "watcher excludes active confirm lease", "AND (state <> 'detected' OR lease_until IS NULL OR lease_until <= $4)", "",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (deposits, "watcher owns due unresolved confirm", "AND NOT (state = 'detected' AND final_at IS NULL", "AND NOT (false AND state = 'detected' AND final_at IS NULL",
     ["--test", "finality", "unresolved_detected_deposit_reverses_only_on_watcher_replacement_proof"]),
    (finality, "confirm positive evidence handoff", "if resume_confirm && matches!(applied, Applied::Nothing | Applied::Followed) {", "if false && matches!(applied, Applied::Nothing | Applied::Followed) {",
     ["--test", "pump", "positive_reincluded_provisional_transfer_returns_to_confirm_without_extra_watcher_reads"]),
    (scanner, "coverage persisted checkpoint conflict", "end == checkpoint.number && (a.0 != checkpoint.hash || b.0 != checkpoint.hash)", "false",
     ["--test", "scanner", "dual_agreed_coverage_boundary_conflict_freezes_before_any_publication"]),
    (scanner, "coverage persisted coverage conflict", "end == cursor.number && (a.0 != cursor.hash || b.0 != cursor.hash)", "false",
     ["--test", "scanner", "dual_agreed_coverage_boundary_conflict_freezes_before_any_publication"]),
    (scanner, "coverage conflict precedes disagreement", "    let (a, b) = tokio::try_join!(read.header(end), verify.header(end))?;", "    let (a, b) = tokio::try_join!(read.header(end), verify.header(end))?;\n    if a != b { return Err(ScannerError::Disagreement); }",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
    (scanner, "coverage read endpoint checkpoint conflict", "a.0 != checkpoint.hash || b.0 != checkpoint.hash", "b.0 != checkpoint.hash",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
    (scanner, "coverage verify endpoint checkpoint conflict", "a.0 != checkpoint.hash || b.0 != checkpoint.hash", "a.0 != checkpoint.hash",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--case", action="append", choices=[case[1] for case in cases], help="Run a named proof; repeat to select several")
parser.add_argument("--hint-only", action="store_true", help="Verify only hint admission mutations")
parser.add_argument("--caps-only", action="store_true", help="Verify only refund attachment cap mutations")
parser.add_argument("--review-fixes-only", action="store_true", help="Verify the three 9056b0a9 review fixes")
parser.add_argument("--finality-only", action="store_true", help="Verify only finality cadence and candidate mutations")
args = parser.parse_args()
if args.hint_only:
    cases = [case for case in cases if case[0] == api]
if args.caps_only:
    cases = [case for case in cases if case[0] == repository]
if args.finality_only:
    cases = [case for case in cases if case[0] in (finality, schedule)]
if args.review_fixes_only:
    cases = [case for case in cases if case[0] in (schedule, deposits, scanner) or case[1] in ("watcher excludes active confirm lease", "confirm positive evidence handoff", "configured staging refund cap", "refund concurrent pending cap", "refund rolling daily cap")]
if args.case:
    cases = [case for case in cases if case[1] in args.case]
if not cases:
    raise SystemExit("no mutation cases selected")
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
        # SQL claim mutations are checked against the prepared disposable schema. This does not
        # rewrite committed metadata; mutated migrations run only in each test's fresh database.
        environment = {**os.environ, "SQLX_OFFLINE": "false"} if path == deposits else None
        result = subprocess.run(command, cwd=root, capture_output=True, timeout=600, env=environment)
        output = result.stdout.decode() + result.stderr.decode()
        if result.returncode == 0 or "test result: FAILED" not in output:
            raise SystemExit(f"mutant survived or did not compile: {name}\n{output}")
        print(f"KILLED: {name}", flush=True)
    finally:
        path.write_text(originals[path])
print(f"PASS: all {len(cases)} behavioral mutants killed; original source restored", flush=True)
