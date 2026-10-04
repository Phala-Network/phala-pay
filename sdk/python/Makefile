# Python SDK tasks. Every target runs the tools pinned in uv.lock.
UV ?= uv
OPENAPI := ../../crates/topup/openapi.json
GENERATED := src/topup_client

.PHONY: sync generate check-generated lint test test-min-python check

sync:
	$(UV) sync --locked --all-groups

generate:
	$(UV) run --locked --group codegen openapi-python-client generate \
		--path $(OPENAPI) \
		--config openapi-python-client.yaml \
		--meta none \
		--output-path $(GENERATED) \
		--overwrite \
		--fail-on-warning

# Regeneration must be a no-op: the committed client always matches the committed OpenAPI spec.
check-generated: generate
	git diff --exit-code -- $(GENERATED)
	test -z "$$(git status --porcelain --untracked-files=all -- $(GENERATED))"

lint:
	$(UV) run --locked ruff check . ../examples ../../deploy/product ../../deploy/sandbox/scenarios ../../deploy/sandbox/smoke.py ../../deploy/local/tls_proxy.py
	$(UV) run --locked ruff format --check . ../examples ../../deploy/product ../../deploy/sandbox/scenarios ../../deploy/sandbox/smoke.py ../../deploy/local/tls_proxy.py
	$(UV) run --locked mypy src/phala_pay src/topup_sdk tests ../examples ../../deploy/product ../../deploy/sandbox/scenarios ../../deploy/sandbox/smoke.py ../../deploy/local/tls_proxy.py

test:
	$(UV) run --locked pytest

# The development interpreter is the newest (.python-version); this exercises the
# requires-python floor in a throwaway environment so the project .venv is left alone.
test-min-python:
	$(UV) run --locked --isolated --python 3.12 pytest -q

check: lint test test-min-python check-generated
