.PHONY: fmt fmt-check clippy test test-pg verify

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

clippy:
	cargo clippy --workspace --all-features --tests -- -D warnings

test:
	cargo test --workspace --all-features

test-pg:
	bash crates/runic-substrate/scripts/test-postgres.sh
	bash crates/runic-serve/scripts/test-postgres.sh

verify: fmt-check clippy test test-pg
