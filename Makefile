.PHONY: run test check budget release

URL ?= https://example.com

run:
	cargo run --release -- $(URL) --tree

test:
	cargo test

# Everything CI runs, in the same order.
check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	./scripts/budget.sh

budget:
	./scripts/budget.sh

release:
	cargo build --release
