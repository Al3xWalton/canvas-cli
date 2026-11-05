.PHONY: all build install test lint fmt

all: build

build:
	cargo build --locked --release

install:
	cargo install --locked --path crates/canvas-cli
	cargo install --locked --path crates/canvas-mcp

test: lint
	cargo test --locked --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all
