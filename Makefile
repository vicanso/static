dev:
	RUST_LOG=INFO cargo watch -w src -x 'run --'


release:
	cargo build --release