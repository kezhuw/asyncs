verify: check build-all test

check: check_fmt lint doc

fmt:
	cargo +nightly fmt --all

check_fmt:
	cargo +nightly fmt --all -- --check

lint:
	cargo clippy --tests --examples --bins --all-features --no-deps -- -D clippy::all

build:
	cargo build --workspace --all-features

build-all:
	cargo build-all-features

test:
	cargo test --workspace --all-features

doc:
	cargo doc --all-features
