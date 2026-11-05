.PHONY: all build install setup-signing test lint fmt

# Local code-signing identity made by `make setup-signing`.
SIGN_ID := canvas-cli local
BIN := $(HOME)/.cargo/bin

all: build

build:
	cargo build --locked --release

install:
	cargo install --locked --path crates/canvas-cli
	cargo install --locked --path crates/canvas-mcp
	@if security find-certificate -c "$(SIGN_ID)" >/dev/null 2>&1; then \
		codesign --force --sign "$(SIGN_ID)" "$(BIN)/canvas" "$(BIN)/canvas-mcp" && \
		echo "Signed canvas and canvas-mcp with \"$(SIGN_ID)\"."; \
	else \
		echo "Not signed: run 'make setup-signing' once so macOS stops re-asking for keychain access."; \
	fi

setup-signing:
	./scripts/setup-signing.sh

test: lint
	cargo test --locked --workspace

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all
