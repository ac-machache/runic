.PHONY: fmt format fmt-check check clippy test test-fast test-doc test-db test-redis test-pg test-stress test-full verify

fmt:
	cargo fmt

format: fmt

fmt-check:
	cargo fmt --check

check:
	cargo check --workspace --all-features --tests

clippy:
	cargo clippy --workspace --all-features --tests -- -D warnings

test: test-fast

test-fast:
	cargo nextest run --workspace -E 'not binary(postgres_contract) and not (package(runic-serve) and kind(test))'

test-doc:
	cargo test --workspace --doc

test-db:
	bash crates/runic-substrate/scripts/test-postgres.sh
	bash crates/runic-serve/scripts/test-postgres.sh

test-redis:
	bash crates/runic-serve/scripts/test-redis.sh


test-pg: test-db

test-stress:
	cargo nextest run --workspace --profile full --run-ignored only -E 'not binary(postgres_contract) and not binary(postgres_api)'
	bash crates/runic-substrate/scripts/test-postgres.sh --ignored

test-full:
	cargo nextest run --workspace --all-features --profile full --run-ignored all
	cargo test --workspace --all-features --doc -- --include-ignored
	bash crates/runic-substrate/scripts/test-postgres.sh --include-ignored
	bash crates/runic-serve/scripts/test-postgres.sh
	bash crates/runic-serve/scripts/test-redis.sh

verify: fmt-check clippy test-fast test-doc test-db test-redis
