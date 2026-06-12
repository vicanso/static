dev:
	STATIC_PATH=./assets RUST_LOG=INFO cargo watch -w src -x 'run --'

fmt:
	cargo fmt

bloat:
	cargo bloat --release 

lint:
	typos
	cargo clippy --all-targets --all -- --deny=warnings

test:
	cargo test

release:
	cargo build --release