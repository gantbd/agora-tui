.PHONY: check fmt lint test doc qualify-daemon

check: fmt lint test doc

fmt:
	cargo fmt --all -- --check

lint:
	cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings

test:
	cargo test --workspace --all-targets --all-features --locked --offline

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked --offline

qualify-daemon:
	sh tests/live/real_daemon.sh
