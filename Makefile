.PHONY: run test check budget startup sites release

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
	./scripts/startup.sh
	./scripts/sites.sh

budget:
	./scripts/budget.sh

startup:
	./scripts/startup.sh

sites:
	./scripts/sites.sh

release:
	cargo build --release
