# Python phase 2 SDK contract audit

Scope: Python phase 2 of [the accepted design](../../docs/design/sdk-ergonomics.md) and
[its normative reference](../../docs/design/sdk-ergonomics-reference.md). Every test reference
below names an existing executable test function, including its parametrized cases. Paths and
line numbers are relative to `sdk/python/`. The table was checked against Python AST definitions.

Setup CLI flags, setup persistence/recovery, copied merchant transaction recipes and their line
counts/paid-flow acceptance belong to phase 3 and are outside this PR. This table makes no claim
that those acceptance tests ran. JS-only rules are handled by the separate JS branch. Python
network work in async routes remains a merchant responsibility (the design uses a synchronous
route/worker thread); this PR does not introduce asynchronous SDK calls.

The generator remains `openapi-python-client==0.29.1`, pinned in `pyproject.toml`/`uv.lock`.
`make -C sdk/python check` runs Ruff, mypy, Python 3.14 tests, isolated Python 3.12 tests and
`check-generated`; regeneration must leave `src/topup_client` unchanged. The generated public
models are retained, rather than replaced with handwritten wire types. Snapshot readonly fields
use a JSON-serializable `TypedDict` with `ReadOnly` annotations (checked by mypy).

| Reference rule | Implemented (file:line) | Test (file:line::test_name) |
|---|---|---|
| Resource methods: quotes | `src/phala_pay/_client.py:319` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: deposit_addresses | `src/phala_pay/_client.py:544` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: deposits | `src/phala_pay/_client.py:440` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: refunds | `src/phala_pay/_client.py:657` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: payment_settings | `src/phala_pay/_client.py:270` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: config | `src/phala_pay/_client.py:304` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: balance | `src/phala_pay/_client.py:782` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: account | `src/phala_pay/_client.py:215` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: treasuries | `src/phala_pay/_client.py:896` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: api_keys | `src/phala_pay/_client.py:1035` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: webhook_endpoints | `src/phala_pay/_client.py:1126` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: events | `src/phala_pay/_client.py:1233` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| Resource methods: sweeps and forwarders list | `src/phala_pay/_client.py:797` | `tests/test_ergonomics_resources.py:154::test_every_list_page_passes_limit_cursor_and_deadline` |
| Keyword controls: every POST idempotency_key and per-call request_deadline | `src/topup_sdk/client.py:1701` | `tests/test_ergonomics_resources.py:117::test_every_resource_method_uses_single_transport_and_explicit_controls` |
| api_keys omitted permissions creates secret; list creates restricted | `src/topup_sdk/client.py:853` | `tests/test_ergonomics_resources.py:185::test_omitted_permissions_create_secret_key_and_lists_create_restricted_keys` |
| Generated models: null/absent, metadata clear, expandable resources | `src/topup_sdk/client.py:449` | `tests/test_ergonomics_resources.py:213::test_nullable_absent_metadata_clears_and_expandables_are_preserved` |
| Unknown response fields/statuses; exact decimal strings and Python int64 | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:665::test_query_and_path_parameters_are_encoded_and_python_int64_is_preserved` |
| Signing and sweep builders remain offline | `src/topup_sdk/treasury.py:20`; `src/topup_sdk/sweeps.py:59` | `tests/test_treasury.py:26::test_the_signature_recovers_to_the_treasury`; `tests/test_sweeps.py:32::test_flush_calldata_is_the_abi_encoding_of_the_factory_call` |
| One UUID per logical POST; frozen body and key across attempts | `src/topup_sdk/_transport.py:69` | `tests/test_ergonomics_transport.py:264::test_post_replay_freezes_body_and_generates_one_uuid_per_invocation` |
| Explicit order keys survive client restart | `src/topup_sdk/client.py:1701` | `tests/test_ergonomics_transport.py:312::test_explicit_order_key_survives_client_restart` |
| Idempotency header length/ASCII validation before IO | `src/topup_sdk/client.py:1831` | `tests/test_ergonomics_transport.py:302::test_idempotency_key_validation_precedes_io` |
| Encode path/query input | `src/topup_sdk/client.py:449` | `tests/test_ergonomics_transport.py:665::test_query_and_path_parameters_are_encoded_and_python_int64_is_preserved` |
| No DELETE retry (status or network failure) | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:356::test_delete_is_never_retried_even_on_network_failure` |
| Reject unreplayable POST streams before authenticated IO | `src/topup_sdk/_transport.py:69` | `tests/test_ergonomics_transport.py:786::test_unreplayable_post_body_is_rejected_before_authenticated_io` |
| Retry network failures, 429, 500/502/503/504, only 409 idempotency_key_in_use | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:323::test_network_failures_retry_then_raise_public_transport_error_without_cause`; `tests/test_ergonomics_transport.py:211::test_only_documented_retry_statuses_and_409_code_retry`; `tests/test_ergonomics_transport.py:231::test_other_statuses_and_conflicts_are_terminal` |
| Replayed success and errors end retries | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:244::test_replayed_responses_end_retries` |
| Half/full jitter of 0.5/1/2 s, subsequent cap 5 s; injected RNG/clock | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:101::test_retry_jitter_half_full_exponential_and_five_second_cap` |
| Retry-After seconds and HTTP-date are exact minimum waits | `src/topup_sdk/client.py:1900` | `tests/test_ergonomics_transport.py:126::test_retry_after_is_exact_minimum_wait` |
| Retry-After beyond remaining budget returns last error without early retry | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:156::test_retry_after_exceeding_remaining_budget_returns_last_error` |
| Attempt timeout includes response body and stream cleanup | `src/topup_sdk/_transport.py:37` | `tests/test_ergonomics_transport.py:399::test_attempt_timeout_includes_response_body_and_closes_stream` |
| Logical deadline includes attempts/sleeps and per-call override | `src/topup_sdk/_transport.py:69` | `tests/test_ergonomics_transport.py:418::test_deadline_includes_attempts_sleeps_and_per_call_override` |
| Python interrupts are terminal | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:343::test_python_interrupts_are_terminal` |
| Finite positive timeouts; integer attempts 1..10 | `src/topup_sdk/client.py:213` | `tests/test_ergonomics_transport.py:373::test_transport_options_fail_before_io`; `tests/test_ergonomics_transport.py:379::test_attempts_are_bounded_integers` |
| HTTPS-only origins, no credentials/query/fragment/prefix; explicit test loopback HTTP | `src/topup_sdk/_origin.py:10` | `tests/test_ergonomics_trust.py:194::test_origins_reject_non_https_credentials_query_fragment_and_prefix`; `tests/test_ergonomics_trust.py:199::test_origin_normalization_override_and_test_loopback_are_fail_closed` |
| Authenticated 301/302/303/307/308 never issue a second request | `src/topup_sdk/_transport.py:69` | `tests/test_ergonomics_transport.py:196::test_authenticated_redirect_is_never_followed` |
| Context cleanup closes owned client; injected transport stays open | `src/topup_sdk/_transport.py:26` | `tests/test_ergonomics_transport.py:713::test_context_manager_closes_client_without_closing_injected_transport` |
| list iterators; api_keys/treasuries retain lists; all list_page methods/controls | `src/topup_sdk/client.py:1591` | `tests/test_ergonomics_resources.py:154::test_every_list_page_passes_limit_cursor_and_deadline` |
| list/list_page reject empty continuing page with ResponseValidationError | `src/topup_sdk/client.py:1564` | `tests/test_ergonomics_transport.py:570::test_all_lists_reject_empty_continuing_pages` |
| list/list_page reject repeated cursor, including cycles across several pages | `src/topup_sdk/client.py:1564`; `src/topup_sdk/client.py:1591` | `tests/test_ergonomics_transport.py:592::test_lists_reject_repeated_cursor`; `tests/test_ergonomics_transport.py:814::test_iterator_rejects_long_cursor_cycles_before_yielding_duplicate_items` |
| Pagination limit boundary before IO | `src/topup_sdk/client.py:1290` | `tests/test_ergonomics_transport.py:802::test_pagination_limit_is_validated_before_io` |
| Verify open quotes/active addresses on every page/action | `src/topup_sdk/client.py:1564` | `tests/test_ergonomics_transport.py:632::test_quotes_and_address_verification_runs_on_every_page_and_action` |
| ApiError snake_case fields and null optional fields | `src/topup_sdk/errors.py:32` | `tests/test_ergonomics_transport.py:467::test_api_errors_preserve_all_optional_fields` |
| Public TransportError timeout/network and no raw httpx cause | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:323::test_network_failures_retry_then_raise_public_transport_error_without_cause` |
| Malformed success response: ResponseValidationError/status/request ID; no raw body | `src/topup_sdk/client.py:1722` | `tests/test_ergonomics_transport.py:448::test_malformed_response_has_public_validation_error_and_no_body` |
| API key/client secret redaction in reprs, exception fields/messages and captured logs | `src/topup_sdk/_secrets.py:25`; `src/topup_sdk/errors.py:32` | `tests/test_ergonomics_transport.py:724::test_api_keys_and_client_secrets_are_redacted_in_reprs_errors_and_logs`; `tests/test_ergonomics_trust.py:494::test_webhook_quote_secret_does_not_leak_in_event_repr` |
| Canonical unpadded base64url/unused bits, UTF-8 object, <=16 KiB | `src/phala_pay/_pins.py:86` | `tests/test_ergonomics_trust.py:67::test_pins_envelope_rejects_format_padding_unused_bits_utf8_duplicates_and_size`; `tests/test_ergonomics_trust.py:74::test_pins_accept_json_key_order_and_exact_size_boundary` |
| Pins reject unknown format and missing/unknown/duplicate fields (recursive) | `src/phala_pay/_pins.py:131` | `tests/test_ergonomics_trust.py:127::test_pins_unknown_missing_and_recursive_duplicate_fields` |
| Compact recursive ASCII key sort including textual chains; lowercase addresses; key versions sorted | `src/phala_pay/_pins.py:140` | `tests/test_ergonomics_trust.py:144::test_pins_encoding_sorts_textual_chain_keys_versions_and_lowercases_addresses` |
| Account acct_+32 lowercase hex; nonzero 20-byte addresses; safe canonical chain IDs | `src/phala_pay/_pins.py:36` | `tests/test_ergonomics_trust.py:119::test_pins_field_boundaries` |
| Webhook keys nonempty, unique public keys/positive uint32 versions, canonical whpk_+32 bytes | `src/phala_pay/_pins.py:36` | `tests/test_ergonomics_trust.py:119::test_pins_field_boundaries` |
| Boolean mode; sk/rk test/live key length, base62 CRC32 checksum and mode binding | `src/phala_pay/_pins.py:158` | `tests/test_ergonomics_trust.py:217::test_api_key_format_checksum_and_mode` |
| Normalize host/scheme/default port/root slash; override must match origin pins | `src/phala_pay/_pins.py:36` | `tests/test_ergonomics_trust.py:199::test_origin_normalization_override_and_test_loopback_are_fail_closed` |
| Frozen Pins, nested input copy, readonly pay.pins/pay.livemode | `src/phala_pay/_pins.py:27` | `tests/test_ergonomics_trust.py:166::test_pins_are_frozen_and_copy_nested_input_and_client_properties_are_readonly` |
| Only two environment values; no trust/file/network discovery or legacy merge; reject mixed constructor | `src/phala_pay/_client.py:155` | `tests/test_ergonomics_trust.py:236::test_from_env_reads_exactly_two_values_and_never_merges_legacy_trust` |
| Missing pins fail at construction in both modes | `src/phala_pay/_client.py:80` | `tests/test_ergonomics_trust.py:217::test_api_key_format_checksum_and_mode` |
| Preserve legacy test warnings and explicit bound-call compatibility; no live address fallback | `src/phala_pay/_client.py:80`; `src/topup_sdk/client.py:1669` | `tests/test_fastapi_example.py:80::test_topup_returns_the_client_secret_and_keys_the_quote_by_order`; `tests/test_phala_pay.py:458::test_construct_event_returns_the_typed_deposit`; `tests/test_client.py:218::test_live_address_checks_fail_closed_without_every_pin` |
| Response account/mode identity must match pins | `src/topup_sdk/client.py:1789` | `tests/test_ergonomics_trust.py:262::test_response_identity_rejects_wrong_mode_and_account` |
| Recompute from pinned account/contracts/chain treasury; reject treasury substitution/missing chain | `src/topup_sdk/client.py:1611`; `src/topup_sdk/client.py:1634`; `src/topup_sdk/client.py:1684` | `tests/test_client.py:236::test_a_spoofed_treasury_with_its_valid_address_is_refused_in_live_mode`; `tests/test_phala_pay.py:383::test_a_deposit_address_the_account_cannot_derive_is_refused` |
| Active address summary agrees with every network; historical closed/retired objects not payable | `src/topup_sdk/client.py:1634` | `tests/test_ergonomics_trust.py:514::test_active_address_summary_empty_networks_and_historical_objects` |
| Bound webhook uses only pinned client keys/account/mode, exact UTF-8/original bytes, header case | `src/phala_pay/_client.py:1331` | `tests/test_ergonomics_trust.py:360::test_bound_webhooks_use_only_pins_accept_text_and_unknown_types_stay_raw` |
| Reject duplicate signing headers | `src/topup_sdk/webhooks.py:129` | `tests/test_ergonomics_trust.py:323::test_bound_webhooks_reject_duplicate_signing_headers` |
| Ed25519 v1a + ID/account/mode/envelope/resource validation; SignatureVerificationError | `src/phala_pay/_client.py:1331` | `tests/test_ergonomics_trust.py:339::test_bound_webhooks_fail_closed_for_invalid_identity_envelope_and_resource` |
| Typed verified Event/event.deposit, inclusive bilateral 300 s and exact zero tolerance | `src/phala_pay/_client.py:1331` | `tests/test_ergonomics_trust.py:299::test_bound_webhook_inclusive_bilateral_window_and_zero_exact_match` |
| Invalid bound tolerance is ConfigurationError | `src/phala_pay/_client.py:1331` | `tests/test_ergonomics_trust.py:316::test_bound_webhook_invalid_tolerance_is_configuration_error` |
| Unknown event types (including future deposit.*) retain raw objects | `src/phala_pay/_webhook.py:176` | `tests/test_ergonomics_trust.py:360::test_bound_webhooks_use_only_pins_accept_text_and_unknown_types_stay_raw` |
| Explicit manual overlap pins/retiring-key notices; notices never update trust; attestation binding preserved | `src/phala_pay/_client.py:1331`; `src/topup_sdk/attestation.py:38` | `tests/test_ergonomics_trust.py:372::test_bound_webhook_rotation_requires_explicit_pins_and_notices_do_not_change_trust`; `tests/test_phala_pay.py:604::test_construct_event_accepts_either_pinned_key_during_a_rotation`; `tests/test_attestation.py:86::test_bindings_that_do_not_match_are_rejected` |
| checkout_params refuses non-open quotes | `src/phala_pay/_client.py:179` | `tests/test_ergonomics_transport.py:510::test_checkout_params_refuses_non_open_quotes` |
| checkout_params refuses missing/null/empty client_secret; retrieve invents no secret | `src/phala_pay/_client.py:179` | `tests/test_ergonomics_transport.py:521::test_checkout_params_refuses_missing_client_secret` |
| checkout_params requires originating verified object, rechecks pins/address and returns exactly browser params | `src/phala_pay/_client.py:179` | `tests/test_ergonomics_transport.py:531::test_checkout_params_requires_originating_verified_quote_and_rechecks_pins` |
| Ledger snapshot schema/JSON, exact pure arithmetic and no input mutation; duplicate/reorder convergence | `src/phala_pay/_ledger.py:88` | `tests/test_ergonomics_trust.py:446::test_ledger_is_pure_json_serializable_and_converges_on_all_delivery_orders` |
| Invalid identity/status/negative/noninteger/unsafe amounts and excess deductions rejected | `src/phala_pay/_ledger.py:34` | `tests/test_ergonomics_trust.py:425::test_ledger_rejects_invalid_identity_status_integers_and_deductions` |
| Merge rejects conflicting identity/customer/currency/mode/valuation | `src/phala_pay/_ledger.py:88` | `tests/test_ergonomics_trust.py:433::test_ledger_merge_rejects_conflicting_identity_and_valuation` |
| Pending < credited/rejected < reversed, max deductions, credited vs rejected conflict | `src/phala_pay/_ledger.py:88` | `tests/test_ergonomics_trust.py:469::test_ledger_pending_rejected_reversal_and_null_to_valued_rules` |
| Net only credited/reversed; credited needs amount; unvalued deductions zero; full valued reversal; no refund+reversal | `src/phala_pay/_ledger.py:74` | `tests/test_ergonomics_trust.py:469::test_ledger_pending_rejected_reversal_and_null_to_valued_rules` |
| Null can become valued once, including previously unvalued reversal; future statuses never credit | `src/phala_pay/_ledger.py:88` | `tests/test_ergonomics_trust.py:502::test_unvalued_reversal_remains_zero_when_valuation_arrives` |
| Execute shared addresses fixtures without changing fixture data | `src/topup_sdk/addresses.py:49` | `tests/test_shared_fixtures.py:54::test_address_fixtures_match_existing_derivation` |
| Execute shared pins fixtures without changing fixture data | `src/phala_pay/_pins.py:86` | `tests/test_shared_fixtures.py:113::test_pins_fixtures_execute` |
| Execute shared ledger fixtures without changing fixture data | `src/phala_pay/_ledger.py:88` | `tests/test_shared_fixtures.py:129::test_ledger_fixtures_execute` |
| Execute shared transport fixtures without changing fixture data | `src/topup_sdk/client.py:1701` | `tests/test_shared_fixtures.py:206::test_transport_fixtures_execute_with_mock_transport` |
| Execute shared webhook vectors through bound client | `src/phala_pay/_client.py:1331` | `tests/test_ergonomics_trust.py:531::test_shared_webhook_vectors_execute_through_bound_client` |
