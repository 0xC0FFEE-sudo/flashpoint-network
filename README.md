# Flashpoint Network (Prototype)
## Flashpoint Network Prototype

This repository contains a Python FastAPI prototype and a Rust Axum server that implement a mock private mempool, simple block building algorithms (FIFO and greedy), observability, and optional feature modules.

### Quickstart (Python app)

1. Install and run:

```
make run
```

2. Visit the dashboard at `http://127.0.0.1:8000/`.

3. Run tests:

```
make test
```

### Rust Axum server

- Build and run:

```
make server-build
make server-run
```

- Run tests:

```
make server-test
make server-test-all-features
```

- Benchmarks and examples:

```
make server-bench
make server-build-examples
make server-load-insert
make server-load-build
```

#### Persistence (sled)

- Default DB path when `FPN_DB_PATH` is unset: `rust/server/data/fpn_<port>` where `<port>` is the server port (from `FPN_PORT`, default `8080`). This per-port default prevents cross-test DB contention and readiness races.
- Override the path by setting `FPN_DB_PATH`:

  ```bash
  # Example: custom absolute path
  FPN_DB_PATH=/tmp/fpn_db cargo run --manifest-path rust/server/Cargo.toml

  # Example: per-run temp dir
  FPN_DB_PATH=$(mktemp -d) cargo test --manifest-path rust/server/Cargo.toml
  ```

- Clean persisted data created by local runs/tests:

  ```bash
  make server-clean-data
  ```

- ABCI shim:

```
# Build shim binary
make abci-shim-build

# Run shim (defaults: FPN_BASE=http://127.0.0.1:8080, FPN_ABCI_PORT=26658)
FPN_BASE=http://127.0.0.1:8080 FPN_ABCI_PORT=26658 make abci-shim-run

# Integration test (spawns server + shim and checks forwarding)
make abci-shim-test
```

### Feature flags (Rust server)

Enable with `--features`, or all at once with `--all-features`.

- `abci`: ABCI++ adapters and endpoints
  - POST `/abci/prepare_proposal` { algorithm, fee_rate, max_intents? }
  - POST `/abci/process_proposal` { algorithm, fee_rate }

- `mev`: Minimal auction book
  - POST `/auction/submit` { client_id?, order } -> { ok, id }
  - POST `/auction/submit_batch` [ { client_id?, order }, ... ] -> { ok, ids }
  - DELETE `/auction/cancel/:id` -> { ok, removed }
  - GET `/auction/order/:id` -> { id, client_id?, order, submitted_at_ms }
  - GET `/auction/book` -> { submissions, top: [ { id, client_id?, bid }, ... ], last_cleared }
  - POST `/auction/clear?top_k=K&min_bid?=F&budget_cap?=F&window_ms?=N&priority_first?=bool`
    - Filters:
      - min_bid: reject bids below this reserve
      - budget_cap: cumulative budget cap across winners
      - window_ms: only consider orders submitted within the last N ms
      - priority_first: sort by ascending `priority` then by `bid` desc (else by `bid` desc only)
    - Returns: { cleared, winners, criteria }
  - GET `/auction/results` -> { submissions, clients, winners }

- `scheduler`: Background frequent batch auction that emits WS events
  - Env: `FPN_SCHED_INTERVAL_MS` (default 1000), `FPN_SCHED_FEE_RATE` (default 0.1)

- `zk`: ZK proving (stub or arkworks Groth16 BN254 backend)
  - Backend selection via env: `FPN_ZK_BACKEND=stub|ark|arkworks|groth16|bn254` (default `stub`)
  - POST `/zk/prove` { claim?, x?, y?, z? } -> { proof_id, claim }
  - POST `/zk/prove_async` { claim?, x?, y?, z? } -> { job_id }
  - GET `/zk/status/{job_id}` -> { job_id, state: pending|done, proof_id?, claim? }
  - If backend is arkworks, the server generates a Groth16 BN254 proof for the circuit x * y = z and verifies it before returning a short hex `proof_id` derived from the proof bytes. If inputs are omitted, defaults are used (x=3, y=4, z=12).

### Observability

- Prometheus metrics at `/metrics` with eagerly registered counters/gauges/histograms.
- JSON metrics at `/metrics/json`.
- WebSocket at `/ws` broadcasting events: `intent_submitted`, `intents_bulk_submitted`, `block_built`, `reset`, and feature events.
  - MEV events: `auction_submitted` { id, client_id?, trace_id }, `auction_canceled` { id, trace_id }, `auction_cleared` { cleared, trace_id }
- Grafana dashboard JSON under `monitoring/grafana/dashboards/fpn_server_observability.json` includes base panels and feature-specific panels.

### Rate limiting and backpressure

Configurable via environment variables:

- `FPN_CONCURRENCY` (default 1024): tower ConcurrencyLimitLayer
- `FPN_BUFFER_SIZE` (default 1024): tower BufferLayer backlog size
- `FPN_REQS_PER_SEC` (default very high): tower RateLimitLayer (global)
- `FPN_WS_CHANNEL_CAP` (default 100): WS broadcast channel capacity; exported as `fpn_ws_channel_capacity`

Overload rejections return `503` and increment `fpn_rejects_total`.

### Authentication

If `FPN_API_KEY` is set, protected endpoints (`/reset`, `/intents*`, `/build_block`, `/seed` and feature endpoints) require header `X-API-Key: <value>`.

### OpenTelemetry

W3C `traceparent` is propagated. To export spans via OTLP HTTP set `OTEL_EXPORTER_OTLP_ENDPOINT` (e.g., `http://127.0.0.1:4318`).

### Development tips

- Lint: `make server-lint`
- Format: `make server-fmt`
- Tokio console: `make server-run-console` and `make tokio-console`
- Flamegraph (if installed): `make server-flamegraph`

### Profiling (tokio-console & flamegraph)

- Tokio Console (async runtime profiling):
  1) Terminal A: `make server-run-console` (sets `RUSTFLAGS="--cfg tokio_unstable"` and starts server with `FPN_TOKIO_CONSOLE=1`).
  2) Terminal B: `make tokio-console` (launch UI). Inspect tasks, wakers, resources.

- Flamegraph (CPU profiling):
  - Install once: `cargo install flamegraph` (provides `cargo flamegraph`).
  - Quick run: `make server-flamegraph` (profiles `fpn_server` and opens the SVG result). Use `RUST_LOG=warn` to reduce tracing noise.
  - Recommended workflow to capture build-mode hot path:
    1) Terminal A (server under flamegraph on a dedicated port):
       ```bash
       FPN_PORT=8085 RUST_LOG=warn cargo flamegraph --bin fpn_server --manifest-path rust/server/Cargo.toml
       ```
    2) Terminal B (drive load for ~15s):
       ```bash
       FPN_LOAD_TARGET=http://127.0.0.1:8085 \
       FPN_LOAD_MODE=build \
       FPN_LOAD_CONCURRENCY=32 \
       FPN_LOAD_RPS=200 \
       FPN_LOAD_DURATION_SECS=15 \
       cargo run --manifest-path rust/server/Cargo.toml --release --example loader
       ```
    3) Stop Terminal A with Ctrl-C to finalize the flamegraph (`flamegraph.svg`).

- Latency percentiles under load (no Docker):
  1) Start the server in one terminal: `make server-run`
  2) In another terminal, run the loader example in insert or build mode:
     - Insert mode (default):
       ```bash
       FPN_LOAD_TARGET=http://127.0.0.1:8080 \
       FPN_LOAD_MODE=insert \
       FPN_LOAD_CONCURRENCY=64 \
       FPN_LOAD_RPS=500 \
       FPN_LOAD_DURATION_SECS=30 \
       make server-load-insert
       ```
     - Build mode (greedy, fee_rate=0.1):
       ```bash
       FPN_LOAD_TARGET=http://127.0.0.1:8080 \
       FPN_LOAD_MODE=build \
       FPN_LOAD_CONCURRENCY=32 \
       FPN_LOAD_RPS=200 \
       FPN_LOAD_DURATION_SECS=30 \
       make server-load-build
       ```
  3) The loader prints p50/p95/p99 in milliseconds at the end. Use this to document scheduler fairness/perf and guide optimizations.

A phased, runnable prototype for the Flashpoint Network based on the provided specs. This repository includes:

- Phase 1: Off-chain simulator (Python) to evaluate sequencing algorithms and prove value vs FIFO.
- Mock Private Mempool API (FastAPI) to submit intents and produce blocks using selectable algorithms.
- Minimal Python SDK client for submitting intents and requesting block proposals.
- Tests and CLI tooling.
- Lightweight Web Dashboard (static HTML/CSS/JS) to interact with the API from your browser.
- Advanced features: WebSocket live updates, Prometheus scrape endpoint, optional API key auth, CORS, and optional OpenTelemetry instrumentation.

This prototype is designed to be immediately runnable without installing blockchain or ZK toolchains. It establishes foundations to iterate towards the Cosmos SDK chain, Execution Nodes, and ZK Signal layer in later phases.

## Repository Structure

- `sim/` — Simulation engine, algorithms, and CLI.
- `mempool/` — FastAPI server acting as a private intent mempool + block producer (mock).
  - `mempool/static/` — Simple web UI served at `/`.
- `sdk/python/` — Minimal Python SDK for interacting with the mock mempool API.
- `tests/` — Unit tests for algorithms and end-to-end checks.
- `zk/` — ZK circuit placeholder (spec + example circuit to be expanded later).
- `examples/` — Example scripts demonstrating SDK usage.
- `howtobuild.md`, `projectdetails.md`, `spec.md` — Original planning/spec docs.

## Quickstart

1) Create and activate a virtual environment (recommended):

```bash
python3 -m venv .venv
source .venv/bin/activate
```

2) Install dependencies:

```bash
pip install -r requirements.txt
```

3) Run tests:

```bash
pytest -q
```

4) Start the mock private mempool API:

```bash
uvicorn mempool.app:app --reload
```

Then open http://127.0.0.1:8000/ to use the dashboard or http://127.0.0.1:8000/docs for interactive API docs.

5) Try the simulator CLI:

```bash
python -m sim.cli compare --n 10 --conflict-rate 0.4
```

This prints total profit under FIFO vs Greedy (conflict-aware) ordering.

6) Run the SDK example against the running server:

```bash
python examples/seed_and_compare.py
```

## API Overview

- `GET /health` — liveness.
- `DELETE /reset` — clear mempool state.
- `GET /intents` — list intents.
- `POST /intents` — submit one intent: `{ profit: number, resources: string[], client_id?: string }`.
- `POST /intents/bulk` — submit many at once: `{ intents: SubmitIntent[] }`.
- `GET /compare` — returns FIFO vs Greedy total profits.
- `GET /build_block?algorithm=greedy|fifo&fee_rate=0..1` — builds block, returns baseline FIFO profit, total profit, surplus, sequencer fee, and per-intent payouts.
- `GET /metrics` — summary stats: intent count, distinct resources, baseline FIFO profit, greedy profit, surplus.
- `GET /metrics/prometheus` — Prometheus text format metrics (histograms, counters, gauges) for scraping.
- `GET /ws` — WebSocket endpoint that broadcasts events: `intent_submitted`, `intents_bulk_submitted`, `block_built`, `reset`.
  - MEV events: `auction_submitted`, `auction_canceled`, `auction_cleared`.
  - ABCI events (if `abci` feature): `abci_prepare_proposal`, `abci_process_proposal`.
  - Scheduler events (if `scheduler` feature): `scheduled_block_built`.

MEV endpoints (if `mev` feature):
- `POST /auction/submit` — submit one order: `{ client_id?: string, order: any }` -> `{ ok: bool, id: u64 }`
- `POST /auction/submit_batch` — submit many orders: `[{ client_id?: string, order: any }, ...]` -> `{ ok: bool, ids: u64[] }`
- `DELETE /auction/cancel/:id` — cancel one order by id -> `{ ok: bool, removed: number }`
- `GET /auction/order/:id` — fetch order by id -> `{ id, client_id?, order, submitted_at_ms }`
- `GET /auction/book` — snapshot -> `{ submissions, top, last_cleared }`
- `POST /auction/clear` with optional query params:
  - `top_k` (default 1)
  - `min_bid` (reserve), `budget_cap` (cumulative), `window_ms` (recency), `priority_first` (true -> sort by ascending `priority` then bid desc)
  - Returns `{ cleared, winners, criteria }`

Example usage (curl):

```bash
# Submit a single order
curl -sS -X POST http://127.0.0.1:8080/auction/submit \
  -H 'content-type: application/json' \
  -d '{"client_id":"alice","order":{"bid":5.5,"priority":2}}'

# Batch submit
curl -sS -X POST http://127.0.0.1:8080/auction/submit_batch \
  -H 'content-type: application/json' \
  -d '[{"client_id":"bob","order":{"bid":3.0}},{"client_id":"carol","order":{"bid":7.0,"priority":1}}]'

# Get specific order by ID
curl -sS http://127.0.0.1:8080/auction/order/1

# Book snapshot (top bids)
curl -sS http://127.0.0.1:8080/auction/book

# Clear with filters: top_k=2, reserve, budget cap, recent window, priority-first
curl -sS -X POST 'http://127.0.0.1:8080/auction/clear?top_k=2&min_bid=2.0&budget_cap=8.0&window_ms=60000&priority_first=true'

# Cancel an order by ID
curl -sS -X DELETE http://127.0.0.1:8080/auction/cancel/1

# ZK (arkworks) sync prove using provided inputs (x*y=z)
FPN_ZK_BACKEND=ark \
curl -sS -X POST http://127.0.0.1:8080/zk/prove \
  -H 'content-type: application/json' \
  -d '{"x":3, "y":4, "z":12, "claim":"mul"}'

# ZK (arkworks) async prove and poll status
FPN_ZK_BACKEND=ark \
curl -sS -X POST http://127.0.0.1:8080/zk/prove_async \
  -H 'content-type: application/json' \
  -d '{"x":5, "y":6, "z":30}'
# Suppose job id is ABC; then:
curl -sS http://127.0.0.1:8080/zk/status/ABC
```

Security (optional):
- Set env var `FPN_API_KEY` to require an API key on mutating endpoints (`/intents`, `/intents/bulk`, `/build_block`, `/reset`).
- Send header `X-API-Key: <your key>` with those requests. The dashboard includes an API key field in the header bar.

Web Dashboard (served at `/`) provides:
- Submit single/bulk intents
- List intents
- Compare FIFO vs Greedy
- Build block with configurable `algorithm` and `fee_rate`
- View metrics
- WebSocket status indicator and live updates as intents/blocks change
- Link to Prometheus `/metrics` for scraping

Observability:
- Prometheus metrics exposed (Python): `/metrics/prometheus`.
- Prometheus metrics exposed (Rust server): `/metrics` (counters, gauges, histograms; scrape with Prometheus).
- Optional OpenTelemetry traces (Python via FastAPI instrumentation; Rust via OTLP exporter). Set `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318` to enable.

## Roadmap Alignment

- Phase 1 (this repo):
  - Algorithms: FIFO vs conflict-aware greedy (approx MWIS) selecting non-conflicting intents to maximize profit.
  - Simulator + tests to validate advantage over FIFO.
  - Mock API + SDK to mirror `PrepareProposal`/`ProcessProposal` flow.
  - Dashboard to visualize flows and economics (baseline vs surplus, sequencer fee split).
  - Added WebSockets for real-time flow, Prometheus metrics, and optional API key auth for mutation endpoints.

- Phase 2+:
  - Replace mock API with Cosmos SDK app-chain (via Ignite CLI scaffold) and implement `PrepareProposal`/`ProcessProposal`.
  - Evolve SDK to speak Cosmos gRPC/Web.
  - Add Execution Node interface and WASM sandbox exploration.
  - Integrate frequent batch auction / fair sequencing (Aequitas-like) policies in `sim/` and app-side mempool.
  - Explore orderflow auctions (MEV-Share concepts) and PBS-aligned builder interfaces.

- Phase 3+:
  - ZK Signal Intelligence: Circom circuit(s) and on-chain verification contracts in a target L2/L1.
  - Evaluate zkVMs (RISC Zero, SP1) for proof of off-chain optimal execution heuristics.

## Notes

- This is a prototype optimized for demonstration and iteration speed.
- Production implementation will require rigorous audits, incentive tuning, and operational hardening.

## Local Observability (no Docker)

We removed Docker/Compose from the workflow. You can run Prometheus and (optionally) Grafana locally:

- Prometheus
  - macOS (Homebrew):
    ```bash
    brew install prometheus
    prometheus --config.file=monitoring/prometheus.yml
    # UI: http://127.0.0.1:9090  |  Target: localhost:8000/metrics/prometheus
    ```
  - Linux: download from https://prometheus.io/download/ and run with the same `--config.file` flag.

- Grafana (optional)
  - macOS (Homebrew):
    ```bash
    brew install grafana
    brew services start grafana
    # UI: http://127.0.0.1:3000  (default login: admin / admin)
    ```
  - Add a Prometheus datasource pointing to `http://localhost:9090` via Grafana UI.
  - The files in `monitoring/grafana/provisioning/` were used for Docker-based provisioning and are kept as references only.

Container configs (`Dockerfile`, `docker-compose.yml`) are deprecated and retained only for reference.

## Optional Rust Accelerator (PyO3)

An optional Rust engine (`fpn_engine`) can accelerate `build_block`. The Python code automatically falls back to pure Python if the Rust module is not present or fails to load.

Build and enable:

```bash
make engine-build            # installs maturin in venv and builds the PyO3 module
export FPN_USE_RUST=1        # opt-in to use the Rust accelerator
make run                     # start the API
```

Disable or skip (if Cargo lock/compile issues occur):

```bash
unset FPN_USE_RUST           # fallback to Python is automatic
make run
```

Notes:
- Results are identical to the Python path; only performance differs.
- If you never build the engine, the app continues to work with Python.

## Rust Axum Server (Phase B)

A standalone Rust HTTP server mirrors the core Python API for ultra-low-latency serving.

Prerequisites (macOS):

```bash
# Install Rust toolchain (recommended via rustup)
brew install rustup-init
rustup-init -y
source "$HOME/.cargo/env"   # ensure cargo is on your PATH in this shell
```

Build and run:

```bash
make server-build
# Optional env:
#   export FPN_PORT=8090          # default 8080
#   export FPN_API_KEY=devsecret  # protects mutation endpoints
#   export FPN_WS_CHANNEL_CAP=100 # WS broadcast channel capacity (default 100)
#   export FPN_CONCURRENCY=1024   # max concurrent in-flight requests (default 1024)
#   export FPN_BUFFER_SIZE=1024   # middleware buffer size (default 1024)
#   export FPN_REQS_PER_SEC=100   # global rate limit (req/s); unset = effectively unlimited
#   export FPN_TOKIO_CONSOLE=1    # enable Tokio console subscriber (off by default)
#   export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318  # enable tracing export
make server-run   # listens on 0.0.0.0:${FPN_PORT:-8080}
```

Endpoints (parity with Python app):
- GET  /health
- DELETE /reset
- GET/POST /intents
- POST /intents/bulk
- GET  /build_block?algorithm=greedy&fee_rate=0.1
- GET  /metrics (Prometheus text format)
- GET  /metrics/json (compact JSON metrics)
- GET  /ws (WebSocket broadcasting)
- GET  /compare
- POST /seed

WebSocket broadcasting:
- Connect clients to `/ws` to receive broadcast JSON messages:
  - `{"type":"intent_submitted", "intent": Intent}`
  - `{"type":"intents_bulk_submitted", "count": N}`
  - `{"type":"block_built", "result": BlockResponse}`
  - `{"type":"reset"}`
- Example (using websocat):
  ```bash
  websocat ws://127.0.0.1:8080/ws
  ```

Security (optional API key):
- Set `FPN_API_KEY` to require header `x-api-key: <value>` on protected routes: `/reset`, `/intents`, `/intents/bulk`, `/build_block`.
- Public: `/health`, `/metrics`, `/ws`.

Observability (Rust):
- Prometheus metrics at `/metrics`:
  - Counters: `fpn_intents_total`, `fpn_build_block_total`
  - Gauge: `fpn_intents_in_mempool`, `fpn_ws_clients`, `fpn_ws_channel_capacity`
  - Histogram: `fpn_build_block_latency_seconds`
  - Counter: `fpn_ws_lagged_total`, `fpn_rejects_total`

OpenTelemetry traces: set `OTEL_EXPORTER_OTLP_ENDPOINT` to enable exporting via OTLP HTTP; uses Tokio batch processor.

### Tuning Guidance for WS Broadcast Channel

- `FPN_WS_CHANNEL_CAP`: Controls how many broadcast messages are buffered per receiver before lag occurs. Increase to tolerate brief spikes; too large increases memory and worst-case latency. Exposed as `fpn_ws_channel_capacity` (set at startup).
- `FPN_CONCURRENCY`: Upper bound on simultaneous in-flight requests; decrease to shed load earlier, increase for higher parallelism if CPU allows.
- `FPN_BUFFER_SIZE`: Size of the tower buffer before load shedding; higher smooths bursts at the cost of memory and tail latency.
- `FPN_REQS_PER_SEC`: Global rate limit; set to cap request rate. Leaving unset disables rate limiting by default.

### Parity and Golden Tests (Rust)

Order-insensitive parity tests compare Rust server responses to golden JSON outputs generated from the Python implementation.

- Golden files live at `rust/server/tests/golden/`
- Tests normalize optional fields (e.g., `client_id` may be omitted/null) and compare floats approximately to avoid brittleness.

Run tests:

```bash
make server-test
# or
cargo test --manifest-path rust/server/Cargo.toml
# related:
make server-test-integration   # only tests/ integration tests
make server-test-ignored       # any ignored tests
```

### cURL Examples (Rust server)

```bash
# Health
curl -s http://127.0.0.1:${FPN_PORT:-8080}/health | jq

# Submit one intent (optionally add: -H "x-api-key: $FPN_API_KEY")
curl -s -X POST http://127.0.0.1:${FPN_PORT:-8080}/intents \
  -H 'content-type: application/json' \
  -d '{"profit": 12.3, "resources": ["pair:ETH/USDC"], "client_id": "demo"}' | jq

# Submit bulk intents
curl -s -X POST http://127.0.0.1:${FPN_PORT:-8080}/intents/bulk \
  -H 'content-type: application/json' \
  -d '{"intents": [{"profit": 10, "resources": ["dex:uniswap"]}, {"profit": 20, "resources": ["pair:ETH/USDC"]}]}' | jq

# List intents
curl -s http://127.0.0.1:${FPN_PORT:-8080}/intents | jq

# Build block (algorithm greedy, fee_rate 0.1)
curl -s "http://127.0.0.1:${FPN_PORT:-8080}/build_block?algorithm=greedy&fee_rate=0.1" | jq

# Metrics (Prometheus text format)
curl -s http://127.0.0.1:${FPN_PORT:-8080}/metrics
```

### WebSocket Walkthrough

Use websocat to observe live events broadcast by the server (`intent_submitted`, `intents_bulk_submitted`, `block_built`, `reset`).

```bash
websocat ws://127.0.0.1:${FPN_PORT:-8080}/ws
# In another shell, submit an intent and watch the event appear.
```

Each event includes a `trace_id` when tracing is enabled for easier correlation across logs and traces.

### OpenTelemetry (no Docker)

You can run a local OTLP collector and export traces from the Rust server without containers.

1) Start a collector using the provided example config:

```bash
otelcol-contrib --config monitoring/otel-collector.yaml
# or: otelcol --config monitoring/otel-collector.yaml
```

2) Run the Rust server with OTLP export enabled:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
make server-run-otel
```

The example config logs spans to stdout. You can adjust exporters (e.g., Tempo/Jaeger) in `monitoring/otel-collector.yaml`.

### Validation & Limits

- `/intents`: `profit > 0`, `resources` non-empty, at most 64 resources/intent
- `/intents/bulk`: non-empty list, at most 1000 intents/request
- `/build_block`: `algorithm` in `{fifo, greedy}`, `fee_rate` in `[0.0, 1.0]`

On validation errors, the active OpenTelemetry span is marked with status `Error` for better observability.

### Grafana Dashboard (optional)

An importable Grafana dashboard is provided at:

`monitoring/grafana/dashboards/fpn_server_observability.json`

It charts:
- WebSocket clients gauge (`fpn_ws_clients`)
- WS channel capacity (stat) (`fpn_ws_channel_capacity`)
- WS lagged rate (`rate(fpn_ws_lagged_total[5m])`)
- Rejects rate (`rate(fpn_rejects_total[5m])`)
- Intent and build_block request rates
- build_block latency quantiles (p50/p95/p99) via Prometheus `histogram_quantile()`

Import it via Grafana UI (Dashboards -> Import) and select your Prometheus datasource.

### WS Lag Demo

Demonstrate the effect of `FPN_WS_CHANNEL_CAP` on lagged drops and visualize metrics.

1) Run the server with a small channel capacity (e.g., 10):

```bash
export FPN_WS_CHANNEL_CAP=10
make server-run
```

2) In another terminal, optionally connect a WebSocket client to observe events:

```bash
websocat ws://127.0.0.1:${FPN_PORT:-8080}/ws
```

3) In a third terminal, run the demo for ~20s (drives load via the loader example and polls metrics):

```bash
make ws-lag-demo
```

Watch terminal output for `fpn_ws_lagged_total` increasing when channel capacity is too small. Open the Grafana dashboard to see:
- WS Channel Capacity (constant, from startup)
- WS Lagged Rate (1/s)
- Rejects Rate (1/s)
- WS Clients

Re-run with a larger capacity (e.g., `FPN_WS_CHANNEL_CAP=100`) to compare.

## SDK

Instantiate with optional API key:

```python
from sdk.python.client import FlashpointClient

client = FlashpointClient(api_key="devkey")
client.reset()
client.submit_intent(profit=12.3, resources=["pair:ETH/USDC"]) 
blk = client.build_block("greedy", fee_rate=0.12)
print(blk)
```

Note: Container configs (`Dockerfile`, `docker-compose.yml`) are deprecated and retained only for reference.
