.PHONY: build build-topup test lint image up down verify-image \
	restore-drill deploy-check runbook-check sdk-check sdk-generate sandbox-local \
	cvm-rehearsal

build:
	cargo build --workspace --locked

# The runtime binary alone, as the image builds it, so no tool-only feature is unified into it.
build-topup:
	cargo build --release --locked -p topup

test:
	cargo test --workspace --locked

# CI's lint job runs exactly this.
lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked --all-features -- -D warnings
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo deny --locked check

image:
	docker build --build-arg SOURCE_DATE_EPOCH="$${SOURCE_DATE_EPOCH:-0}" -t phala-pay:dev .
	docker run --rm phala-pay:dev topup --help

# The attested compose rendered from the local environment, plus the local overlay (Garage S3,
# dstack simulator, images built here).
LOCAL_COMPOSE = deploy/local/compose.sh -p phala-pay-local

up:
	$(LOCAL_COMPOSE) up --build -d postgres dstack-simulator backup

down:
	$(LOCAL_COMPOSE) down --remove-orphans

verify-image:
	deploy/verify-image.sh

restore-drill:
	deploy/local/restore-drill.sh

cvm-rehearsal:
	deploy/local/cvm-rehearsal.sh

deploy-check:
	cd contracts && forge fmt --check
	cd contracts && forge build
	cd contracts && forge test
	deploy/contracts/check-build.sh --check
	deploy/contracts/reference-manifest.sh --check
	deploy/contracts/test-determinism.sh

runbook-check:
	deploy/runbooks/check.sh

sdk-check:
	$(MAKE) -C sdk/python sync check

sdk-generate:
	$(MAKE) -C sdk/python generate
	npx -y pnpm@12.6.0 --dir sdk/js run generate:types

sandbox-local:
	deploy/sandbox/run-local.sh
