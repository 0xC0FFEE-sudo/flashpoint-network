# Flashpoint Network Prototype - Makefile

PYTHON ?= python3
VENV ?= .venv
UVICORN := $(VENV)/bin/uvicorn
PYTEST := $(VENV)/bin/pytest
PIP := $(VENV)/bin/pip

.PHONY: help venv install run test clean engine-help engine-install engine-build engine-clean server-help server-build server-run server-test server-clean ws-lag-demo

help:
	@echo "Targets:"
	@echo "  make install       # create venv and install deps"
	@echo "  make run           # run FastAPI app with reload"
	@echo "  make test          # run test suite"
	@echo "  make clean         # remove venv and caches"
	@echo "  make engine-help   # show optional Rust engine targets"
	@echo "  make server-help   # show Rust Axum server targets"
	@echo "  make ws-lag-demo   # run WS lag demo (requires server running)"
	@echo "\nNote: Docker/Compose targets have been removed. Run locally with venv."

engine-help:
	@echo "Optional Rust engine (PyO3) targets:"
	@echo "  make engine-install  # install maturin into the venv"
	@echo "  make engine-build    # build and install fpn_engine into the venv"
	@echo "  make engine-clean    # remove rust build artifacts"

$(VENV):
	$(PYTHON) -m venv $(VENV)

install: $(VENV)
	$(PIP) install --upgrade pip
	$(PIP) install -r requirements.txt

run: install
	$(UVICORN) mempool.app:app --host 0.0.0.0 --port 8000 --reload

test: install
	$(PYTEST) -q

clean:
	rm -rf $(VENV) __pycache__ **/__pycache__ .pytest_cache

# --- Optional Rust engine (PyO3) ---
ENGINE_MANIFEST := rust/engine/Cargo.toml
MATURIN := $(VENV)/bin/maturin
CARGO := cargo

engine-install: install
	$(PIP) install maturin

engine-build: engine-install
	$(MATURIN) develop --manifest-path $(ENGINE_MANIFEST)
	@echo "Built fpn_engine. Enable with: export FPN_USE_RUST=1"

engine-clean:
	rm -rf rust/engine/target

# --- Rust Axum server (Phase B) ---
SERVER_MANIFEST := rust/server/Cargo.toml

server-help:
	@echo "Rust Axum server targets:"
	@echo "  make server-build    # cargo build the Rust server"
	@echo "  make server-run      # cargo run the Rust server on :8080 (override with FPN_PORT)"
	@echo "  make server-test     # cargo test the Rust server"
	@echo "  make server-test-integration # cargo test --tests (integration tests under tests/)"
	@echo "  make server-test-ignored # cargo test -- --ignored (if any ignored tests exist)"
	@echo "  make server-fmt      # cargo fmt (format the Rust server)"
	@echo "  make server-lint     # cargo clippy -D warnings (lint the Rust server)"
	@echo "  make server-test-watch  # watch tests (requires cargo-watch if installed)"
	@echo "  make server-run-otel # run with OTLP exporter enabled (set OTEL_EXPORTER_OTLP_ENDPOINT)"
	@echo "  make server-build-release # cargo build --release"
	@echo "  make server-bench    # cargo bench (builds release binary first)"
	@echo "  make server-bench-all-features # cargo bench with --all-features"
	@echo "  make server-run-console # run with tokio-console subscriber enabled"
	@echo "  make tokio-console   # launch tokio-console UI (if installed)"
	@echo "  make server-flamegraph # run cargo flamegraph (if installed)"
	@echo "  make server-build-examples # cargo build --release --examples (build async loader)"
	@echo "  make server-load-insert   # run async loader (insert mode); requires server running"
	@echo "  make server-load-build    # run async loader (build mode); requires server running"
	@echo "  make server-clean    # remove server target artifacts"
	@echo "  make abci-shim-build # build ABCI shim binary"
	@echo "  make abci-shim-run   # run ABCI shim (FPN_BASE, FPN_ABCI_PORT configurable)"
	@echo "  make abci-shim-test  # run ABCI shim integration test"

server-build:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST)

server-run:
	RUST_LOG=info $(CARGO) run --manifest-path $(SERVER_MANIFEST)

server-test:
	$(CARGO) test --manifest-path $(SERVER_MANIFEST)

server-test-integration:
	$(CARGO) test --manifest-path $(SERVER_MANIFEST) --tests

server-test-ignored:
	$(CARGO) test --manifest-path $(SERVER_MANIFEST) -- --ignored

server-fmt:
	cargo fmt --manifest-path $(SERVER_MANIFEST) --all

server-lint:
	cargo clippy --manifest-path $(SERVER_MANIFEST) --all-targets --all-features -- -D warnings

# Best-effort test watch: uses cargo-watch if installed; otherwise, prints a hint
server-test-watch:
	@if command -v cargo-watch >/dev/null 2>&1; then \
		cargo watch -w rust/server -x 'test --manifest-path rust/server/Cargo.toml'; \
	else \
		echo "cargo-watch not found. Install with: cargo install cargo-watch"; \
	fi

server-run-otel:
	@if [ -z "$$OTEL_EXPORTER_OTLP_ENDPOINT" ]; then \
		echo "Set OTEL_EXPORTER_OTLP_ENDPOINT (e.g. http://127.0.0.1:4318) before running"; \
	fi
	RUST_LOG=info $(CARGO) run --manifest-path $(SERVER_MANIFEST)

server-clean:
	rm -rf rust/server/target

server-build-release:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST) --release

server-bench:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST) --release
	$(CARGO) bench --manifest-path $(SERVER_MANIFEST)

.PHONY: server-bench-all-features
server-bench-all-features:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST) --release --all-features
	$(CARGO) bench --manifest-path $(SERVER_MANIFEST) --all-features

server-run-console:
	RUSTFLAGS="--cfg tokio_unstable" FPN_TOKIO_CONSOLE=1 RUST_LOG=info $(CARGO) run --manifest-path $(SERVER_MANIFEST) --release

tokio-console:
	@if command -v tokio-console >/dev/null 2>&1; then \
		tokio-console; \
	else \
		echo "tokio-console not found. Install with: cargo install tokio-console"; \
	fi

server-flamegraph:
	@if command -v cargo-flamegraph >/dev/null 2>&1; then \
		cargo flamegraph --bin fpn_server --manifest-path $(SERVER_MANIFEST); \
	else \
		echo "cargo-flamegraph not found. Install with: cargo install flamegraph"; \
	fi

server-build-examples:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST) --release --examples

# --- Convenience: test with all feature flags ---
.PHONY: server-test-all-features
server-test-all-features:
	$(CARGO) test --manifest-path $(SERVER_MANIFEST) --all-features

server-load-insert:
	FPN_LOAD_MODE=insert RUST_LOG=warn $(CARGO) run --manifest-path $(SERVER_MANIFEST) --release --example loader

server-load-build:
	FPN_LOAD_MODE=build RUST_LOG=warn $(CARGO) run --manifest-path $(SERVER_MANIFEST) --release --example loader

# --- ABCI shim ---
.PHONY: abci-shim-build abci-shim-run abci-shim-test
abci-shim-build:
	$(CARGO) build --manifest-path $(SERVER_MANIFEST) --bin abci_shim

abci-shim-run:
	RUST_LOG=info $(CARGO) run --manifest-path $(SERVER_MANIFEST) --bin abci_shim

abci-shim-test:
	$(CARGO) test --manifest-path $(SERVER_MANIFEST) --all-features --test abci_shim_integration

# --- Monitoring helpers ---
ws-lag-demo:
	bash monitoring/scripts/ws_lag_demo.sh 20
