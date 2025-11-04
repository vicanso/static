dev:
	STATIC_PATH=~/Downloads RUST_LOG=INFO cargo watch -w src -x 'run --'

lint:
	typos
	cargo clippy --all-targets --all -- --deny=warnings

release:
	cargo build --release