build:
    cargo build --release

fmt:
    cargo fmt

lint:
    cargo clippy --workspace --all-targets

test:
    cargo test --workspace

check: fmt lint test
    cargo build --release
