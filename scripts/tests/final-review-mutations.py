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
checkpoint = root / "crates/topup/src/checkpoint.rs"
confirmation = root / "crates/topup/src/db/confirmation.rs"
bounds = root / "crates/topup/migrations/20261103000000_confirmation_bounds.up.sql"
confirm_step = root / "crates/topup/src/steps/confirm.rs"
metrics = root / "crates/topup/src/observability/metrics.rs"
originals = {path: path.read_text() for path in [refunds, api, repository, finality, schedule, deposits, scanner, checkpoint, confirmation, bounds, confirm_step, metrics]}
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
    (deposits, "confirm must use shared backoff", '             finality_check_at=finality_next_check_at(COALESCE(first_unresolved_at,$2),$2) \\\n', "             finality_check_at=$2 + interval '2 seconds' \\\n",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (deposits, "confirm unresolved persistence", "if writes.effects.first_unresolved {", "if false {",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (finality, "watcher once-only unresolved entry", "AND first_unresolved_at IS NULL AND lease_token=$3", "AND lease_token=$3",
     ["--test", "pump", "absent_confirm_and_watcher_share_one_schedule_for_twenty_four_hours"]),
    (confirmation, "shared reader excludes active lease", "AND (lease_until IS NULL OR lease_until <= $3 OR lease_token=$2)", "AND true",
     ["--test", "confirmation_bounds", "missed_positions_restart_crash_and_stale_claims_never_reset_budgets"]),
    (deposits, "watcher owns due unresolved confirm", "first_unresolved_at IS NULL AND confirm_receipt_checks=0", "true",
     ["--test", "finality", "unresolved_detected_deposit_reverses_only_on_watcher_replacement_proof"]),
    (finality, "confirm positive evidence handoff", "if deposit.state == DepositState::Detected && terminal_transfer {", "if false && terminal_transfer {",
     ["--test", "pump", "positive_reincluded_provisional_transfer_returns_to_confirm_without_extra_watcher_reads"]),
    (scanner, "coverage persisted checkpoint conflict", "end == checkpoint.number && contradicts(checkpoint.hash)", "false",
     ["--test", "scanner", "dual_agreed_coverage_boundary_conflict_freezes_before_any_publication"]),
    (scanner, "coverage persisted coverage conflict", "end == cursor.number && contradicts(cursor.hash)", "false",
     ["--test", "scanner", "dual_agreed_coverage_boundary_conflict_freezes_before_any_publication"]),
    (scanner, "coverage conflict precedes disagreement", "    let successful = [a.as_ref().ok(), b.as_ref().ok()];", "    if matches!((&a, &b), (Ok(a), Ok(b)) if a != b) { return Err(ScannerError::Disagreement); }\n    let successful = [a.as_ref().ok(), b.as_ref().ok()];",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
    (scanner, "coverage read endpoint checkpoint conflict", "let successful = [a.as_ref().ok(), b.as_ref().ok()];", "let successful = [None, b.as_ref().ok()];",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
    (scanner, "coverage verify endpoint checkpoint conflict", "let successful = [a.as_ref().ok(), b.as_ref().ok()];", "let successful = [a.as_ref().ok(), None];",
     ["--test", "scanner", "single_endpoint_coverage_boundary_conflict_freezes_before_disagreement"]),
]
cases += [
    (confirmation, "Depth six probes and fixed last position", "[0, 4, 12, 28, 60, 124]", "[0, 4, 12, 28, 60, 123]",
     ["--test", "confirmation_bounds", "normal_modes_and_slow_lane_have_hard_method_bounds"]),
    (confirmation, "Safe fixed cadence", "vec![0, 384, 768]", "vec![0, 60, 120]",
     ["--test", "confirmation_bounds", "normal_modes_and_slow_lane_have_hard_method_bounds"]),
    (confirmation, "Finalized seven probes", "(0..7).map", "(0..8).map",
     ["--test", "confirmation_bounds", "normal_modes_and_slow_lane_have_hard_method_bounds"]),
    (confirmation, "normal deadline immutable", "confirm_deadline_at=COALESCE(confirm_deadline_at,$4)", "confirm_deadline_at=$4 + interval '1 second'",
     ["--test", "confirmation_bounds", "missed_positions_restart_crash_and_stale_claims_never_reset_budgets"]),
    (confirmation, "receipt quota once only", "AND confirm_receipt_checks=0", "AND confirm_receipt_checks<=1",
     ["--test", "confirmation_bounds", "missed_positions_restart_crash_and_stale_claims_never_reset_budgets"]),
    (confirmation, "read result version CAS", "AND updated_at=$3 AND state='detected' \\\n         AND confirm_receipt_checks=0", "AND state='detected' \\\n         AND confirm_receipt_checks=0",
     ["--test", "confirmation_bounds", "missed_positions_restart_crash_and_stale_claims_never_reset_budgets"]),
    (confirm_step, "Depth anchor uses earlier endpoint", "now.max(ea.min(eb))", "now.max(ea.max(eb))",
     ["--test", "confirmation_bounds", "normal_modes_and_slow_lane_have_hard_method_bounds"]),
    (confirm_step, "changed receipt enters S", "unresolved(\"confirmation_evidence_changed\")", "result",
     ["--test", "confirmation_bounds", "receipt_anomalies_and_price_failure_keep_watcher_ownership_and_one_quota"]),
    (bounds, "old final S watcher eligibility", "(deposit.state = 'detected'", "(deposit.state = 'detected' AND deposit.final_at IS NULL",
     ["--test", "finality", "old_final_detected_absence_keeps_s_until_fresh_terminal_proof"]),
    (bounds, "nonterminal proof cannot be cached", "AND evidence -> 'chain_confirmation' -> 'terminal' = 'true'::jsonb", "AND true",
     ["--test", "confirmation_bounds", "terminal_reference_requires_new_schema_and_complete_terminal_identity"]),
    (finality, "successor inherits consumed receipt quota", "confirm_receipt_checks=GREATEST(confirm_receipt_checks,1)", "confirm_receipt_checks=confirm_receipt_checks",
     ["--test", "confirmation_bounds", "nonterminal_price_failure_then_reorg_reverses_with_atomic_successor"]),
]
cases += [
    (confirm_step, "handed evidence token identity", "        && transfer.token == token", "        && { let _ = token; true }",
     ["--test", "confirmation_bounds", "watcher_changed_identity_never_confirms_or_values_original"]),
    (confirm_step, "handed evidence sender identity", "        && transfer.from == from", "        && { let _ = from; true }",
     ["--test", "confirmation_bounds", "watcher_changed_identity_never_confirms_or_values_original"]),
    (confirm_step, "handed evidence amount identity", "        && transfer.amount == amount", "        && { let _ = amount; true }",
     ["--test", "confirmation_bounds", "watcher_changed_identity_never_confirms_or_values_original"]),
    (finality, "watcher terminal handoff identity guard", "deposit.is_same_transfer(transfer)\n                    &&", "transfer.to == deposit.address\n                    &&",
     ["--test", "confirmation_bounds", "watcher_changed_identity_never_confirms_or_values_original"]),
    (confirm_step, "supplied identity rejection", 'return unresolved("supplied_confirmation_identity_changed");', '// supplied identity rejection removed',
     ["--lib", "supplied_evidence_requires_every_transfer_identity_field"]),
    (bounds, "terminal proof freshness", "OR transitions.created_at >= deposits.first_unresolved_at", "OR true",
     ["--test", "confirmation_bounds", "old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused"]),
    (bounds, "terminal proof equality boundary", "transitions.created_at >= deposits.first_unresolved_at", "transitions.created_at > deposits.first_unresolved_at",
     ["--test", "confirmation_bounds", "terminal_proof_freshness_boundary_is_inclusive"]),
    (bounds, "old final watcher admission before S", "OR deposit.final_at IS NOT NULL)", ")",
     ["--test", "confirmation_bounds", "old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused"]),
    (bounds, "N-1 final marker invalidates proof reference", "IF NEW.final_at IS DISTINCT FROM OLD.final_at", "IF FALSE",
     ["--test", "confirmation_bounds", "n_minus_one_final_marker_requires_one_fresh_watcher_proof_then_zero_retry_reads"]),
    (finality, "old final watcher selection without checkpoint", "OR (state='detected' AND final_at IS NOT NULL)", "OR false",
     ["--test", "confirmation_bounds", "old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused"]),
    (confirmation, "price retry retains original proof reference", "WHERE id=$1 AND confirmation_terminal_evidence(id) IS NULL", "WHERE id=$1",
     ["--test", "confirmation_bounds", "terminal_price_retry_keeps_the_normal_method_ceiling"]),
    (deposits, "old final pump fallback excluded", "AND confirm_receipt_checks=0 AND final_at IS NULL", "AND confirm_receipt_checks=0",
     ["--test", "confirmation_bounds", "old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused"]),
    (metrics, "final detected deposit leaves L stock", "AND state='detected' AND final_at IS NULL AND confirm_receipt_checks=0", "AND state='detected' AND confirm_receipt_checks=0",
     ["--test", "confirmation_bounds", "slow_gauges_are_db_derived_once_only_and_keep_resolved_entries"]),
]
cases += [
    (bounds, "terminal proof exact reference", "AND deposits.confirmation_terminal_transition_id = transitions.id", "AND true",
     ["--test", "confirmation_bounds", "old_terminal_proof_is_retired_on_s_and_only_fresh_watcher_evidence_is_reused"]),
    (bounds, "terminal proof new path version", "AND evidence -> 'confirmation_proof_version' = '1'::jsonb", "AND true",
     ["--test", "confirmation_bounds", "terminal_reference_requires_new_schema_and_complete_terminal_identity"]),
    (bounds, "terminal proof complete primary identity", "AND (evidence #> '{chain_confirmation,receipts,0,Included,transfer}')\n          ?& ARRAY['to','token','from','amount','tx_from','tx_nonce']", "AND true",
     ["--test", "confirmation_bounds", "terminal_reference_requires_new_schema_and_complete_terminal_identity"]),
    (bounds, "terminal proof complete secondary identity", "AND (evidence #> '{chain_confirmation,receipts,1,Included,transfer}')\n          ?& ARRAY['to','token','from','amount','tx_from','tx_nonce']", "AND true",
     ["--test", "confirmation_bounds", "terminal_reference_requires_new_schema_and_complete_terminal_identity"]),
]
cases += [
    (confirm_step, "confirmation conflict before peer error", "        let successful_heads = [a.as_ref().ok(), b.as_ref().ok()]", "        if a.is_err() || b.is_err() { return unresolved(\"rpc_failure\"); }\n        let successful_heads = [a.as_ref().ok(), b.as_ref().ok()]",
     ["--test", "confirmation_bounds", "confirmation_boundary_conflict_freezes_even_when_peer_errors"]),
    (scanner, "coverage conflict before peer error", "    let successful = [a.as_ref().ok(), b.as_ref().ok()];", "    if a.is_err() || b.is_err() { return Err(ScannerError::Disagreement); }\n    let successful = [a.as_ref().ok(), b.as_ref().ok()];",
     ["--test", "scanner", "coverage_boundary_conflict_freezes_even_when_peer_errors"]),
    (checkpoint, "checkpoint conflict before peer error", "        if [a.as_ref().ok(), b.as_ref().ok()]", "        if a.is_err() || b.is_err() { return Err(crate::scanner::ScannerError::Disagreement); }\n        if [a.as_ref().ok(), b.as_ref().ok()]",
     ["--test", "scanner", "previous_checkpoint_conflict_freezes_even_when_peer_errors"]),
    (confirm_step, "held handoff retains terminal evidence", "if settings_held && supplied.is_none() {", "if settings_held {",
     ["--test", "confirmation_bounds", "held_terminal_evidence_survives_business_waits_without_further_chain_reads"]),
    (confirm_step, "held cached wait retains effects", "return settings_unconfirmed(effects);", "return settings_unconfirmed(TransitionEffects::default());",
     ["--test", "confirmation_bounds", "held_terminal_evidence_survives_business_waits_without_further_chain_reads"]),
    (finality, "watcher Final requires persisted terminal proof", "if !persisted {", "if false {",
     ["--test", "confirmation_bounds", "watcher_does_not_report_final_when_handoff_does_not_persist_terminal_evidence"]),
    (finality, "watcher propagates stale handoff", "if !matches!(outcome, crate::pump::RunOnceResult::Applied { .. }) {", "if false {",
     ["--test", "confirmation_bounds", "watcher_propagates_stale_handoff_even_when_another_writer_saved_a_proof"]),
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
        environment = {**os.environ, "SQLX_OFFLINE": "false", "DATABASE_URL": os.environ["OWNER_DATABASE_URL"]} if path == deposits else None
        result = subprocess.run(command, cwd=root, capture_output=True, timeout=600, env=environment)
        output = result.stdout.decode() + result.stderr.decode()
        if result.returncode == 0 or "test result: FAILED" not in output:
            raise SystemExit(f"mutant survived or did not compile: {name}\n{output}")
        print(f"KILLED: {name}", flush=True)
    finally:
        path.write_text(originals[path])
print(f"PASS: all {len(cases)} behavioral mutants killed; original source restored", flush=True)
