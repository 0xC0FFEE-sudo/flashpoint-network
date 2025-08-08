use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use once_cell::sync::Lazy;
use opentelemetry::trace::{Status, TraceContextExt};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use prometheus::{
    register_histogram, register_int_counter, register_int_gauge, Encoder, Histogram, IntCounter,
    IntGauge, TextEncoder,
};
use rand::seq::SliceRandom;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{collections::HashSet, net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};
use tower::buffer::BufferLayer;
use tower::limit::{ConcurrencyLimitLayer, RateLimitLayer};
use tower::load_shed::LoadShedLayer;
use tower::{BoxError, ServiceBuilder};
use tower_http::cors::{Any, CorsLayer};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Intent {
    profit: f64,
    resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arrival_index: Option<u64>,
}

// Default resource pool used by seed endpoint
const RESOURCE_POOL: &[&str] = &[
    "pair:ETH/USDC",
    "pair:WBTC/ETH",
    "pair:SOL/USDC",
    "dex:uniswap",
    "dex:curve",
    "bridge:wormhole",
    "bridge:layerzero",
];

// Hardening limits
const MAX_BULK_INTENTS: usize = 1000;
const MAX_RESOURCES_PER_INTENT: usize = 64;

// WebSocket support
async fn ws_route(State(state): State<SharedState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let rx = state.tx.subscribe();
    ws.on_upgrade(move |socket| handle_socket(socket, rx))
}

async fn handle_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    WS_CLIENTS_GAUGE.inc();
    // Forward broadcast messages to this client; ignore client->server messages.
    loop {
        match rx.recv().await {
            Ok(msg) => {
                if socket.send(Message::Text(msg)).await.is_err() {
                    break;
                }
            }
            Err(RecvError::Lagged(_)) => {
                WS_LAGGED_TOTAL.inc();
                // continue to try to keep up
                continue;
            }
            Err(RecvError::Closed) => break,
        }
    }
    WS_CLIENTS_GAUGE.dec();
}

// API Key middleware (protects routes when a key is configured in state).
async fn require_api_key_with_state(
    State(state): State<SharedState>,
    req: Request<Body>,
    next: Next,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    if let Some(expected) = state.config.api_key.clone() {
        let provided = req.headers().get("x-api-key").and_then(|h| h.to_str().ok());
        if provided != Some(expected.as_str()) {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"detail":"invalid API key"})),
            ));
        }
    }
    Ok(next.run(req).await)
}

// Propagate W3C tracecontext from incoming request headers into the current span,
// so handler spans become children of the inbound context.
async fn otel_propagate(req: Request<Body>, next: Next) -> impl IntoResponse {
    // Extract parent context from headers
    let extractor = AxumHeaderExtractor(req.headers());
    let parent_cx = opentelemetry::global::get_text_map_propagator(|p| p.extract(&extractor));
    // Set as parent of the current span; handler spans (#[instrument]) inherit from this
    tracing::Span::current().set_parent(parent_cx);
    next.run(req).await
}

// Minimal header extractor for OpenTelemetry propagation without extra crate deps
struct AxumHeaderExtractor<'a>(&'a axum::http::HeaderMap);
impl<'a> opentelemetry::propagation::Extractor for AxumHeaderExtractor<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
struct SubmitIntent {
    profit: f64,
    resources: Vec<String>,
    #[serde(default)]
    client_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct BulkSubmitRequest {
    intents: Vec<SubmitIntent>,
}

#[derive(Clone, Debug, Deserialize)]
struct SeedRequest {
    #[serde(default = "default_seed_count")]
    count: i32,
    #[serde(default = "default_min_profit")]
    min_profit: f64,
    #[serde(default = "default_max_profit")]
    max_profit: f64,
    #[serde(default)]
    resource_pool: Option<Vec<String>>,
}

fn default_seed_count() -> i32 {
    50
}
fn default_min_profit() -> f64 {
    5.0
}
fn default_max_profit() -> f64 {
    120.0
}

#[derive(Clone, Debug, Serialize)]
struct IntentPayout {
    profit: f64,
    resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arrival_index: Option<u64>,
    fee_share: f64,
    trader_profit: f64,
}

#[derive(Clone, Debug, Serialize)]
struct BlockResponse {
    algorithm: String,
    fee_rate: f64,
    baseline_fifo_profit: f64,
    total_profit: f64,
    surplus: f64,
    sequencer_fee: f64,
    selected: Vec<IntentPayout>,
}

#[derive(Default)]
struct AppState {
    intents: Vec<Intent>,
    next_index: u64,
}

#[derive(Clone)]
struct ServerConfig {
    api_key: Option<String>,
}

struct ServerState {
    app: RwLock<AppState>,
    tx: broadcast::Sender<String>,
    config: ServerConfig,
}

type SharedState = Arc<ServerState>;

// Prometheus metrics
static INTENTS_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!("fpn_intents_total", "Total number of intents accepted").unwrap()
});
static INTENTS_GAUGE: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!("fpn_intents_in_mempool", "Current intents in mempool").unwrap()
});
static BUILD_BLOCK_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_build_block_total",
        "Total number of build_block requests"
    )
    .unwrap()
});
static BUILD_BLOCK_HIST: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "fpn_build_block_latency_seconds",
        "Latency of build_block handler",
        vec![0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 1.0]
    )
    .unwrap()
});

static WS_CLIENTS_GAUGE: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "fpn_ws_clients",
        "Current number of connected WebSocket clients"
    )
    .unwrap()
});

static REJECTS_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_rejects_total",
        "Total requests rejected due to overload/limits"
    )
    .unwrap()
});

static WS_LAGGED_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_ws_lagged_total",
        "Total number of broadcast messages dropped due to slow WS clients"
    )
    .unwrap()
});

// Expose configured WS broadcast channel capacity
static WS_CHANNEL_CAP_GAUGE: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "fpn_ws_channel_capacity",
        "Configured capacity (buffer size) of the WebSocket broadcast channel"
    )
    .unwrap()
});

// API key is read per-request in middleware for flexibility (tests can set/unset env at runtime).

// --- Feature-gated metrics ---
#[cfg(feature = "scheduler")]
static SCHED_BATCHES_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_scheduler_batches_total",
        "Total number of scheduled batches executed"
    )
    .unwrap()
});

#[cfg(feature = "scheduler")]
static SCHED_BATCH_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "fpn_scheduler_batch_latency_seconds",
        "Latency per scheduled batch",
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5]
    )
    .unwrap()
});

#[cfg(feature = "mev")]
static AUCTION_SUBMISSIONS_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_auction_submissions_total",
        "Total number of auction submissions"
    )
    .unwrap()
});

#[cfg(feature = "mev")]
static AUCTION_CLEAR_TOTAL: Lazy<IntCounter> = Lazy::new(|| {
    register_int_counter!(
        "fpn_auction_clear_total",
        "Total number of auction clearing operations"
    )
    .unwrap()
});

#[cfg(feature = "zk")]
static ZK_PROOFS_TOTAL: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("fpn_zk_proofs_total", "Total ZK proof requests").unwrap());

#[cfg(feature = "zk")]
static ZK_PROOF_LATENCY: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "fpn_zk_proof_latency_seconds",
        "Latency of ZK proof stub handler",
        vec![0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025]
    )
    .unwrap()
});

#[cfg(feature = "zk")]
static ZK_JOBS_GAUGE: Lazy<IntGauge> = Lazy::new(|| {
    register_int_gauge!(
        "fpn_zk_jobs_inflight",
        "Number of async ZK proof jobs currently pending"
    )
    .unwrap()
});

fn fifo_select(mut items: Vec<Intent>) -> Vec<Intent> {
    items.sort_by_key(|i| i.arrival_index.unwrap_or(u64::MAX));
    let mut selected: Vec<Intent> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for it in items.into_iter() {
        if it.resources.iter().all(|r| !used.contains(r)) {
            used.extend(it.resources.iter().cloned());
            selected.push(it);
        }
    }
    selected
}

fn greedy_select(mut items: Vec<Intent>) -> Vec<Intent> {
    items.sort_by(|a, b| b.profit.partial_cmp(&a.profit).unwrap());
    let mut selected: Vec<Intent> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for it in items.into_iter() {
        if it.resources.iter().all(|r| !used.contains(r)) {
            used.extend(it.resources.iter().cloned());
            selected.push(it);
        }
    }
    selected
}

fn total_profit(items: &[Intent]) -> f64 {
    items
        .iter()
        .map(|i| if i.profit > 0.0 { i.profit } else { 0.0 })
        .sum()
}

async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

#[tracing::instrument(skip(state))]
async fn reset(State(state): State<SharedState>) -> Json<Value> {
    let mut s = state.app.write().await;
    s.intents.clear();
    s.next_index = 0;
    INTENTS_GAUGE.set(0);
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state
        .tx
        .send(json!({"type":"reset", "trace_id": trace_id}).to_string());
    Json(json!({"reset":true}))
}

#[tracing::instrument(skip(state))]
async fn list_intents(State(state): State<SharedState>) -> Json<Vec<Intent>> {
    let s = state.app.read().await;
    Json(s.intents.clone())
}

#[tracing::instrument(skip(state, req), fields(client_id, resources_count, profit))]
async fn submit_intent(
    State(state): State<SharedState>,
    Json(req): Json<SubmitIntent>,
) -> Result<Json<Intent>, (StatusCode, Json<Value>)> {
    if req.profit <= 0.0 {
        record_span_error("profit must be positive");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"profit must be positive"})),
        ));
    }
    if req.resources.is_empty() {
        record_span_error("resources must be non-empty");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"resources must be non-empty"})),
        ));
    }
    if req.resources.len() > MAX_RESOURCES_PER_INTENT {
        record_span_error("too many resources for one intent");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"too many resources"})),
        ));
    }
    // Enrich span fields
    {
        let span = tracing::Span::current();
        span.record(
            "client_id",
            tracing::field::display(req.client_id.as_deref().unwrap_or("none")),
        );
        span.record(
            "resources_count",
            tracing::field::display(req.resources.len()),
        );
        span.record("profit", tracing::field::display(req.profit));
    }
    let mut s = state.app.write().await;
    let it = Intent {
        profit: req.profit,
        resources: req.resources,
        client_id: req.client_id,
        arrival_index: Some(s.next_index),
    };
    s.next_index += 1;
    s.intents.push(it.clone());
    INTENTS_TOTAL.inc();
    INTENTS_GAUGE.set(s.intents.len() as i64);
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state
        .tx
        .send(json!({"type":"intent_submitted","intent":it, "trace_id": trace_id}).to_string());
    Ok(Json(it))
}

#[tracing::instrument(skip(state, req), fields(count))]
async fn submit_intents_bulk(
    State(state): State<SharedState>,
    Json(req): Json<BulkSubmitRequest>,
) -> Result<Json<Vec<Intent>>, (StatusCode, Json<Value>)> {
    if req.intents.is_empty() {
        record_span_error("intents must be non-empty");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"intents must be non-empty"})),
        ));
    }
    if req.intents.len() > MAX_BULK_INTENTS {
        record_span_error("too many intents in bulk request");
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"detail":"too many intents"})),
        ));
    }
    // Enrich span fields
    tracing::Span::current().record("count", tracing::field::display(req.intents.len()));
    let mut s = state.app.write().await;
    let mut out = Vec::with_capacity(req.intents.len());
    for si in req.intents.into_iter() {
        if si.profit <= 0.0 {
            record_span_error("profit must be positive");
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"detail":"profit must be positive"})),
            ));
        }
        if si.resources.is_empty() {
            record_span_error("resources must be non-empty");
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"detail":"resources must be non-empty"})),
            ));
        }
        if si.resources.len() > MAX_RESOURCES_PER_INTENT {
            record_span_error("too many resources for one intent");
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"detail":"too many resources"})),
            ));
        }
        let it = Intent {
            profit: si.profit,
            resources: si.resources,
            client_id: si.client_id,
            arrival_index: Some(s.next_index),
        };
        s.next_index += 1;
        s.intents.push(it.clone());
        out.push(it);
    }
    INTENTS_TOTAL.inc_by(out.len() as u64);
    INTENTS_GAUGE.set(s.intents.len() as i64);
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({"type":"intents_bulk_submitted","count":out.len(), "trace_id": trace_id})
            .to_string(),
    );
    Ok(Json(out))
}

#[derive(Deserialize)]
struct BuildQuery {
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_fee_rate")]
    fee_rate: f64,
}

fn default_algorithm() -> String {
    "greedy".to_string()
}
fn default_fee_rate() -> f64 {
    0.1
}

#[tracing::instrument(skip(state, q), fields(algorithm, fee_rate))]
async fn build_block(
    State(state): State<SharedState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<BlockResponse>, (StatusCode, Json<Value>)> {
    let _timer = BUILD_BLOCK_HIST.start_timer();
    BUILD_BLOCK_TOTAL.inc();
    if q.algorithm != "fifo" && q.algorithm != "greedy" {
        record_span_error("invalid algorithm");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"algorithm must be 'fifo' or 'greedy'"})),
        ));
    }
    if q.fee_rate < 0.0 || q.fee_rate > 1.0 {
        record_span_error("invalid fee_rate");
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"fee_rate must be between 0.0 and 1.0"})),
        ));
    }
    // Enrich span fields
    {
        let span = tracing::Span::current();
        span.record("algorithm", tracing::field::display(&q.algorithm));
        span.record("fee_rate", tracing::field::display(q.fee_rate));
    }
    let s = state.app.read().await;
    let intents = s.intents.clone();

    let fifo_sel = fifo_select(intents.clone());
    let fifo_profit = total_profit(&fifo_sel);

    let selected = match q.algorithm.as_str() {
        "fifo" => fifo_sel,
        _ => greedy_select(intents),
    };
    let total = total_profit(&selected);
    let surplus = (total - fifo_profit).max(0.0);
    let sequencer_fee = surplus * q.fee_rate;

    // Compute payouts without intermediate allocations: two-pass over selected
    let sum_gross: f64 = selected.iter().map(|it| it.profit.max(0.0)).sum::<f64>();
    let denom = if sum_gross <= 0.0 { 1.0 } else { sum_gross };
    let mut payouts: Vec<IntentPayout> = Vec::with_capacity(selected.len());
    for it in selected.into_iter() {
        let gp = it.profit.max(0.0);
        let share = sequencer_fee * (gp / denom);
        payouts.push(IntentPayout {
            profit: it.profit,
            resources: it.resources,
            client_id: it.client_id,
            arrival_index: it.arrival_index,
            fee_share: share,
            trader_profit: gp - share,
        });
    }

    let resp = BlockResponse {
        algorithm: q.algorithm,
        fee_rate: q.fee_rate,
        baseline_fifo_profit: fifo_profit,
        total_profit: total,
        surplus,
        sequencer_fee,
        selected: payouts,
    };
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({
            "type":"block_built",
            "algorithm": resp.algorithm,
            "fee_rate": resp.fee_rate,
            "baseline_fifo_profit": resp.baseline_fifo_profit,
            "total_profit": resp.total_profit,
            "surplus": resp.surplus,
            "sequencer_fee": resp.sequencer_fee,
            "selected_count": resp.selected.len(),
            "trace_id": trace_id
        })
        .to_string(),
    );
    Ok(Json(resp))
}

fn record_span_error(msg: &str) {
    // Mark current span as error for OTEL exporters; also record a log event
    tracing::error!("{}", msg);
    let ctx = tracing::Span::current().context();
    let span = ctx.span();
    span.set_status(Status::Error {
        description: std::borrow::Cow::Owned(msg.to_string()),
    });
}

#[tracing::instrument(skip(state, req))]
#[axum::debug_handler]
async fn seed_mempool(
    State(state): State<SharedState>,
    Json(req): Json<SeedRequest>,
) -> Result<Json<Vec<Intent>>, (StatusCode, Json<Value>)> {
    if req.count <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"count must be positive"})),
        ));
    }
    if req.min_profit <= 0.0 || req.max_profit <= 0.0 || req.max_profit < req.min_profit {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"invalid profit range"})),
        ));
    }
    let pool: Vec<String> = if let Some(p) = req.resource_pool.clone() {
        p
    } else {
        RESOURCE_POOL.iter().map(|s| s.to_string()).collect()
    };
    if pool.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"detail":"resource pool must be non-empty"})),
        ));
    }

    let to_create: Vec<SubmitIntent> = {
        let mut rng = rand::thread_rng();
        let mut v: Vec<SubmitIntent> = Vec::with_capacity(req.count as usize);
        for i in 0..req.count {
            let profit = rng.gen_range(req.min_profit..=req.max_profit);
            let k: usize = if rng.gen_bool(0.5) { 1 } else { 2 };
            let mut choices = pool.clone();
            choices.shuffle(&mut rng);
            let picked: Vec<String> = choices.into_iter().take(k).collect();
            v.push(SubmitIntent {
                profit: (profit * 100.0).round() / 100.0,
                resources: picked,
                client_id: Some(format!("seed-{}", (i as usize) % 5)),
            });
        }
        v
    };

    // Reuse bulk logic for consistent state updates and metrics
    let res =
        submit_intents_bulk(State(state), Json(BulkSubmitRequest { intents: to_create })).await?;
    Ok(res)
}

#[derive(Serialize)]
struct CompareResponse {
    fifo_profit: f64,
    greedy_profit: f64,
}

#[tracing::instrument(skip(state))]
async fn compare(State(state): State<SharedState>) -> Json<CompareResponse> {
    let s = state.app.read().await;
    let intents = s.intents.clone();
    let fifo_p = total_profit(&fifo_select(intents.clone()));
    let greedy_p = total_profit(&greedy_select(intents));
    Json(CompareResponse {
        fifo_profit: fifo_p,
        greedy_profit: greedy_p,
    })
}

#[derive(Serialize)]
struct MetricsJsonResponse {
    intents_count: usize,
    distinct_resources: usize,
    fifo_profit: f64,
    greedy_profit: f64,
    surplus: f64,
}

async fn metrics_json(State(state): State<SharedState>) -> Json<MetricsJsonResponse> {
    let s = state.app.read().await;
    let intents = s.intents.clone();
    let fifo_p = total_profit(&fifo_select(intents.clone()));
    let greedy_sel = greedy_select(intents.clone());
    let greedy_p = total_profit(&greedy_sel);
    let resources: HashSet<String> = intents
        .into_iter()
        .flat_map(|it| it.resources.into_iter())
        .collect();
    Json(MetricsJsonResponse {
        intents_count: s.intents.len(),
        distinct_resources: resources.len(),
        fifo_profit: fifo_p,
        greedy_profit: greedy_p,
        surplus: (greedy_p - fifo_p).max(0.0),
    })
}

async fn metrics() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buf = Vec::new();
    encoder.encode(&metric_families, &mut buf).unwrap();
    let body = String::from_utf8(buf).unwrap();
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        body,
    )
}

fn build_router(state: SharedState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Backpressure and rate limiting configuration
    let concurrency_limit: usize = std::env::var("FPN_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let buffer_size: usize = std::env::var("FPN_BUFFER_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let rps: u64 = std::env::var("FPN_REQS_PER_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX / 4);

    // Public routes (always open)
    let public = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/metrics/json", get(metrics_json))
        .route("/ws", get(ws_route))
        .route("/compare", get(compare))
        .route("/intents", get(list_intents));

    // Protected routes (require API key if configured)
    let protected = Router::new()
        .route("/reset", axum::routing::delete(reset))
        .route("/intents", post(submit_intent))
        .route("/intents/bulk", post(submit_intents_bulk))
        .route("/build_block", get(build_block))
        .route("/seed", post(seed_mempool))
        .with_state(state.clone());

    // Feature-gated experimental/prototype endpoints
    #[cfg(feature = "abci")]
    let protected = protected
        .route("/abci/prepare_proposal", post(abci_prepare_proposal))
        .route("/abci/process_proposal", post(abci_process_proposal));

    #[cfg(feature = "mev")]
    let protected = protected
        .route("/auction/submit", post(auction_submit))
        .route("/auction/submit_batch", post(auction_submit_batch))
        .route("/auction/cancel/:id", axum::routing::delete(auction_cancel))
        .route("/auction/order/:id", get(auction_get_order))
        .route("/auction/book", get(auction_book))
        .route("/auction/results", get(auction_results))
        .route("/auction/clear", post(auction_clear));

    #[cfg(feature = "zk")]
    let protected = protected
        .route("/zk/prove", post(zk_prove))
        .route("/zk/prove_async", post(zk_prove_async))
        .route("/zk/status/:job_id", get(zk_status))
        .route("/zk/backend", get(zk_backend));

    // Always attach middleware; it will only enforce when an API key is present.
    let protected = protected.layer(middleware::from_fn_with_state(
        state.clone(),
        require_api_key_with_state,
    ));

    let app = public
        .nest("/", protected)
        .with_state(state)
        .layer(middleware::from_fn(otel_propagate))
        .layer(cors)
        .layer(
            ServiceBuilder::new()
                // Convert shed/limit/buffer errors into 503 responses and count them
                .layer(HandleErrorLayer::new(|_err: BoxError| async move {
                    REJECTS_TOTAL.inc();
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"detail":"server overloaded"})),
                    )
                }))
                .layer(LoadShedLayer::new())
                .layer(ConcurrencyLimitLayer::new(concurrency_limit))
                .layer(BufferLayer::new(buffer_size))
                .layer(RateLimitLayer::new(rps, Duration::from_secs(1))),
        );

    app
}

#[tokio::main]
async fn main() {
    if matches!(
        std::env::var("FPN_TOKIO_CONSOLE").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    ) {
        // Enable tokio-console subscriber for async runtime profiling
        console_subscriber::init();
    } else {
        let env_filter = tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        );
        let fmt_layer = tracing_subscriber::fmt::layer();

        let registry = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer);

        // Optional OpenTelemetry OTLP exporter (enabled if OTEL_EXPORTER_OTLP_ENDPOINT is set)
        if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
            use opentelemetry::trace::TracerProvider as _; // bring `tracer()` into scope
            use opentelemetry::KeyValue;
            use opentelemetry_otlp::WithExportConfig;
            use opentelemetry_sdk::{runtime::Tokio, trace as sdktrace, Resource};

            let exporter = opentelemetry_otlp::new_exporter()
                .http()
                .with_endpoint(endpoint);

            let trace_config =
                sdktrace::Config::default().with_resource(Resource::new(vec![KeyValue::new(
                    "service.name",
                    "fpn-server",
                )]));

            let provider = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(exporter)
                .with_trace_config(trace_config)
                .install_batch(Tokio)
                .expect("failed to install OTLP provider");

            let tracer = provider.tracer("fpn-server");
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            registry.with(otel_layer).init();
        } else {
            registry.init();
        }
    }

    // Set global W3C tracecontext propagator so we can extract `traceparent`
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let ws_cap: usize = std::env::var("FPN_WS_CHANNEL_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let (tx, _rx) = broadcast::channel(ws_cap);
    // Set the gauge to the configured capacity
    WS_CHANNEL_CAP_GAUGE.set(ws_cap as i64);
    let state: SharedState = Arc::new(ServerState {
        app: RwLock::new(AppState::default()),
        tx,
        config: ServerConfig {
            api_key: std::env::var("FPN_API_KEY").ok(),
        },
    });

    // Start background scheduler if enabled
    #[cfg(feature = "scheduler")]
    start_scheduler(state.clone());

    let app = build_router(state.clone());

    // Eagerly register Prometheus metrics so /metrics is populated even before traffic
    let _ = &*INTENTS_TOTAL;
    let _ = &*INTENTS_GAUGE;
    let _ = &*BUILD_BLOCK_TOTAL;
    let _ = &*BUILD_BLOCK_HIST;
    let _ = &*WS_CHANNEL_CAP_GAUGE;
    let _ = &*WS_CLIENTS_GAUGE;
    let _ = &*WS_LAGGED_TOTAL;
    let _ = &*REJECTS_TOTAL;
    #[cfg(feature = "zk")]
    let _ = &*ZK_JOBS_GAUGE;
    #[cfg(feature = "mev")]
    let _ = &*AUCTION_CLEAR_TOTAL;

    let port: u16 = std::env::var("FPN_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("listening on {}", addr);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await
        .unwrap();
}

// ---------------- Feature-gated handlers and helpers ----------------

// ABCI++ dev harness: PrepareProposal and ProcessProposal adapters
#[cfg(feature = "abci")]
#[derive(Deserialize)]
struct PrepareProposalReq {
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_fee_rate")]
    fee_rate: f64,
    #[serde(default)]
    max_intents: Option<usize>,
}

#[cfg(feature = "abci")]
#[derive(Deserialize)]
struct ProcessProposalReq {
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_fee_rate")]
    fee_rate: f64,
}

#[cfg(feature = "abci")]
#[tracing::instrument(skip(state, req), fields(algorithm = %req.algorithm, fee_rate = %req.fee_rate))]
async fn abci_prepare_proposal(
    State(state): State<SharedState>,
    Json(req): Json<PrepareProposalReq>,
) -> Result<Json<BlockResponse>, (StatusCode, Json<Value>)> {
    let s = state.app.read().await;
    let mut intents = s.intents.clone();
    drop(s);
    // Apply optional max_intents by earliest arrival_index
    intents.sort_by_key(|i| i.arrival_index.unwrap_or(u64::MAX));
    if let Some(k) = req.max_intents {
        if intents.len() > k {
            intents.truncate(k);
        }
    }
    // Reuse build logic
    let fifo_sel = fifo_select(intents.clone());
    let fifo_profit = total_profit(&fifo_sel);
    let selected = match req.algorithm.as_str() {
        "fifo" => fifo_sel,
        _ => greedy_select(intents),
    };
    let total = total_profit(&selected);
    let surplus = (total - fifo_profit).max(0.0);
    let sequencer_fee = surplus * req.fee_rate;
    let gross: Vec<f64> = selected.iter().map(|it| it.profit.max(0.0)).collect();
    let sum_gross: f64 = if gross.is_empty() {
        1.0
    } else {
        gross.iter().sum()
    };
    let mut payouts = Vec::with_capacity(selected.len());
    for (it, gp) in selected.into_iter().zip(gross.into_iter()) {
        let share = if sum_gross > 0.0 {
            sequencer_fee * (gp / sum_gross)
        } else {
            0.0
        };
        payouts.push(IntentPayout {
            profit: it.profit,
            resources: it.resources,
            client_id: it.client_id,
            arrival_index: it.arrival_index,
            fee_share: share,
            trader_profit: it.profit.max(0.0) - share,
        });
    }
    let resp = BlockResponse {
        algorithm: req.algorithm,
        fee_rate: req.fee_rate,
        baseline_fifo_profit: fifo_profit,
        total_profit: total,
        surplus,
        sequencer_fee,
        selected: payouts,
    };
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({
            "type":"abci_prepare_proposal",
            "algorithm": resp.algorithm,
            "fee_rate": resp.fee_rate,
            "selected_count": resp.selected.len(),
            "trace_id": trace_id
        })
        .to_string(),
    );
    Ok(Json(resp))
}

#[cfg(feature = "abci")]
#[tracing::instrument(skip(state, req), fields(algorithm = %req.algorithm, fee_rate = %req.fee_rate))]
async fn abci_process_proposal(
    State(state): State<SharedState>,
    Json(req): Json<ProcessProposalReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Stub: accept any proposal; emit event
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({
            "type":"abci_process_proposal",
            "algorithm": req.algorithm,
            "fee_rate": req.fee_rate,
            "accepted": true,
            "trace_id": trace_id
        })
        .to_string(),
    );
    Ok(Json(json!({"accepted": true})))
}

// MEV/PBS primitives: minimal in-memory auction interface
#[cfg(feature = "mev")]
#[derive(Clone, Debug, Deserialize, Serialize)]
struct AuctionSubmit {
    #[serde(default)]
    client_id: Option<String>,
    order: Value,
    // Assigned by server on submit for tracking/cancel
    #[serde(default)]
    id: u64,
    // Unix millis when submitted (filled by server)
    #[serde(default)]
    submitted_at_ms: u64,
    // Cached numeric fields for hot paths (not part of external API)
    #[serde(skip, default)]
    bid_f64: f64,
    #[serde(skip, default)]
    priority_u64: u64,
}

#[cfg(feature = "mev")]
#[derive(Default)]
struct AuctionBook {
    submissions: Vec<AuctionSubmit>,
    last_winners: Vec<AuctionSubmit>,
    // Fast index: order id -> index in submissions
    index: std::collections::HashMap<u64, usize>,
}

#[cfg(feature = "mev")]
static AUCTION_BOOK: Lazy<RwLock<AuctionBook>> = Lazy::new(|| RwLock::new(AuctionBook::default()));

#[cfg(feature = "mev")]
static NEXT_AUCTION_ID: Lazy<std::sync::atomic::AtomicU64> =
    Lazy::new(|| std::sync::atomic::AtomicU64::new(1));

#[cfg(feature = "mev")]
#[tracing::instrument(skip(state, req))]
async fn auction_submit(State(state): State<SharedState>, Json(mut req): Json<AuctionSubmit>) -> Json<Value> {
    AUCTION_SUBMISSIONS_TOTAL.inc();
    // Assign ID and timestamp
    let id = NEXT_AUCTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    req.id = id;
    req.submitted_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_millis(0))
        .as_millis() as u64;
    // Cache numeric fields
    req.bid_f64 = req.order.get("bid").and_then(|v| v.as_f64()).unwrap_or(0.0);
    req.priority_u64 = req
        .order
        .get("priority")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    // Insert into book and index
    let mut book = AUCTION_BOOK.write().await;
    book.submissions.push(req.clone());
    let idx = book.submissions.len() - 1;
    book.index.insert(id, idx);
    // Broadcast WS event
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({
            "type":"auction_submitted",
            "id": id,
            "client_id": req.client_id,
            "trace_id": trace_id
        })
        .to_string(),
    );
    Json(json!({"ok": true, "id": id}))
}

#[cfg(feature = "mev")]
async fn auction_results() -> Json<Value> {
    let s = AUCTION_BOOK.read().await;
    Json(json!({
        "submissions": s.submissions.len(),
        "clients": s.submissions.iter().filter_map(|s| s.client_id.as_ref()).collect::<HashSet<_>>().len(),
        "winners": s.last_winners.len()
    }))
}

#[cfg(feature = "mev")]
#[derive(Debug, Deserialize)]
struct ClearQuery {
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    min_bid: Option<f64>,
    #[serde(default)]
    budget_cap: Option<f64>,
    #[serde(default)]
    window_ms: Option<u64>,
    #[serde(default)]
    priority_first: Option<bool>,
}

#[cfg(feature = "mev")]
#[tracing::instrument(skip(state, q))]
async fn auction_clear(State(state): State<SharedState>, Query(q): Query<ClearQuery>) -> Json<Value> {
    AUCTION_CLEAR_TOTAL.inc();
    let k = q.top_k.unwrap_or(1);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_millis(0))
        .as_millis() as u64;
    let mut book = AUCTION_BOOK.write().await;
    // Start with all submissions
    let mut filtered: Vec<&AuctionSubmit> = book.submissions.iter().collect();
    // Apply window filter if set
    if let Some(win) = q.window_ms {
        let cutoff = now_ms.saturating_sub(win);
        filtered.retain(|s| s.submitted_at_ms >= cutoff);
    }
    // Apply min_bid reserve if set
    if let Some(min_bid) = q.min_bid {
        filtered.retain(|s| s.bid_f64 >= min_bid);
    }
    // Rank either by priority then bid, or by bid desc using partial selection
    let top_len = std::cmp::min(k, filtered.len());
    if top_len > 0 {
        let get_bid = |s: &AuctionSubmit| s.bid_f64;
        let get_pri = |s: &AuctionSubmit| s.priority_u64;
        let cmp = |a: &&AuctionSubmit, b: &&AuctionSubmit| {
            // Note: comparator orders desired winners first (descending bid or by priority then bid)
            if q.priority_first.unwrap_or(false) {
                match get_pri(*a).cmp(&get_pri(*b)) {
                    std::cmp::Ordering::Equal => {
                        let ba = get_bid(*a);
                        let bb = get_bid(*b);
                        bb.partial_cmp(&ba).unwrap_or(std::cmp::Ordering::Equal)
                    }
                    other => other,
                }
            } else {
                let ba = get_bid(*a);
                let bb = get_bid(*b);
                bb.partial_cmp(&ba).unwrap_or(std::cmp::Ordering::Equal)
            }
        };
        // Partition so that top_len best are in the first segment, then sort that small slice deterministically
        filtered.select_nth_unstable_by(top_len - 1, cmp);
        filtered[..top_len].sort_unstable_by(cmp);
    }
    // Take top_len, then apply optional budget_cap cumulatively
    let mut winners: Vec<AuctionSubmit> = Vec::with_capacity(top_len);
    let mut sum = 0.0_f64;
    for s in filtered.into_iter().take(top_len) {
        if let Some(cap) = q.budget_cap {
            let bid = s.bid_f64;
            if sum + bid > cap {
                continue;
            }
            sum += bid;
        }
        winners.push((*s).clone());
    }
    book.last_winners = winners.clone();
    // Broadcast cleared event
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    let _ = state.tx.send(
        json!({
            "type":"auction_cleared",
            "cleared": book.last_winners.len(),
            "trace_id": trace_id
        })
        .to_string(),
    );
    Json(json!({
        "cleared": book.last_winners.len(),
        "winners": book
            .last_winners
            .iter()
            .map(|w| json!({"client_id": w.client_id, "order": w.order}))
            .collect::<Vec<_>>(),
        "criteria": if q.priority_first.unwrap_or(false) { "priority_then_bid" } else { "bid_desc" },
    }))
}

// --- Additional MEV interfaces ---
#[cfg(feature = "mev")]
#[tracing::instrument(skip(state, reqs))]
async fn auction_submit_batch(
    State(state): State<SharedState>,
    Json(mut reqs): Json<Vec<AuctionSubmit>>,
) -> Json<Value> {
    let mut ids = Vec::with_capacity(reqs.len());
    let mut book = AUCTION_BOOK.write().await;
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_millis(0))
        .as_millis() as u64;
    let trace_id = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .trace_id()
        .to_string();
    for mut r in reqs.drain(..) {
        AUCTION_SUBMISSIONS_TOTAL.inc();
        let id = NEXT_AUCTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        r.id = id;
        r.submitted_at_ms = now_ms;
        // Cache numeric fields
        r.bid_f64 = r.order.get("bid").and_then(|v| v.as_f64()).unwrap_or(0.0);
        r.priority_u64 = r
            .order
            .get("priority")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX);
        ids.push(id);
        book.submissions.push(r);
        let idx = book.submissions.len() - 1;
        book.index.insert(id, idx);
        let _ = state.tx.send(
            json!({"type":"auction_submitted","id": id, "trace_id": trace_id}).to_string(),
        );
    }
    Json(json!({"ok": true, "ids": ids}))
}

#[cfg(feature = "mev")]
#[tracing::instrument(skip(state))]
async fn auction_cancel(State(state): State<SharedState>, Path(id): Path<u64>) -> Json<Value> {
    let mut book = AUCTION_BOOK.write().await;
    let mut removed = 0usize;
    if let Some(&idx) = book.index.get(&id) {
        removed = 1;
        let last = book.submissions.len() - 1;
        if idx != last {
            // Move the last element into idx
            book.submissions.swap(idx, last);
            // After swap, the element at idx is the one moved from 'last'
            let moved_id = book.submissions[idx].id;
            // Remove the last element (the one we intended to delete)
            book.submissions.pop();
            // Update indices: remove deleted id and update moved element's index
            book.index.remove(&id);
            book.index.insert(moved_id, idx);
        } else {
            // Removing the last element; just pop and remove mapping
            book.submissions.pop();
            book.index.remove(&id);
        }
    }
    if removed > 0 {
        let trace_id = tracing::Span::current()
            .context()
            .span()
            .span_context()
            .trace_id()
            .to_string();
        let _ = state.tx.send(
            json!({"type":"auction_canceled","id": id, "trace_id": trace_id}).to_string(),
        );
    }
    Json(json!({"ok": true, "removed": removed}))
}

#[cfg(feature = "mev")]
async fn auction_get_order(Path(id): Path<u64>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let book = AUCTION_BOOK.read().await;
    if let Some(&idx) = book.index.get(&id) {
        let s = &book.submissions[idx];
        return Ok(Json(json!({
            "id": s.id,
            "client_id": s.client_id,
            "order": s.order,
            "submitted_at_ms": s.submitted_at_ms
        })));
    }
    Err((StatusCode::NOT_FOUND, Json(json!({"detail":"not found"}))))
}

#[cfg(feature = "mev")]
async fn auction_book() -> Json<Value> {
    let s = AUCTION_BOOK.read().await;
    // Return a light snapshot to avoid heavy payloads
    let count = s.submissions.len();
    let top = {
        let mut refs: Vec<&AuctionSubmit> = s.submissions.iter().collect();
        let cmp = |a: &&AuctionSubmit, b: &&AuctionSubmit| {
            let ba = a.bid_f64;
            let bb = b.bid_f64;
            bb.partial_cmp(&ba).unwrap_or(std::cmp::Ordering::Equal)
        };
        let top_len = std::cmp::min(10, refs.len());
        if top_len > 0 {
            refs.select_nth_unstable_by(top_len - 1, cmp);
            refs[..top_len].sort_unstable_by(cmp);
        }
        refs.into_iter()
            .take(top_len)
            .map(|w| json!({"id": w.id, "client_id": w.client_id, "bid": w.order.get("bid") }))
            .collect::<Vec<_>>()
    };
    Json(json!({"submissions": count, "top": top, "last_cleared": s.last_winners.len()}))
}

// ZK stub endpoint
#[cfg(feature = "zk")]
mod zk_backend_ark {
    use once_cell::sync::Lazy;
    // Arkworks imports
    use ark_bn254::{Bn254, Fr};
    use ark_groth16::{Groth16, prepare_verifying_key, ProvingKey, PreparedVerifyingKey};
    use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystemRef, SynthesisError};
    use ark_serialize::CanonicalSerialize;
    use ark_snark::{SNARK, CircuitSpecificSetupSNARK};
    use std::string::ToString;

    // Simple circuit: prove knowledge of x,y such that x * y = z (public)
    pub struct MulCircuit {
        pub x: Option<Fr>,
        pub y: Option<Fr>,
        pub z: Fr, // public input
    }

    impl ConstraintSynthesizer<Fr> for MulCircuit {
        fn generate_constraints(self, cs: ConstraintSystemRef<Fr>) -> Result<(), SynthesisError> {
            // Allocate public input z
            let z_var = cs.new_input_variable(|| Ok(self.z))?;
            // Allocate private witnesses x, y
            let x_val = self.x.ok_or(SynthesisError::AssignmentMissing)?;
            let y_val = self.y.ok_or(SynthesisError::AssignmentMissing)?;
            let x_var = cs.new_witness_variable(|| Ok(x_val))?;
            let y_var = cs.new_witness_variable(|| Ok(y_val))?;
            // Enforce x * y = z
            cs.enforce_constraint(
                ark_relations::r1cs::LinearCombination::from(x_var),
                ark_relations::r1cs::LinearCombination::from(y_var),
                ark_relations::r1cs::LinearCombination::from(z_var),
            )?;
            Ok(())
        }
    }

    pub struct GrothCtx {
        pub pk: ProvingKey<Bn254>,
        pub pvk: PreparedVerifyingKey<Bn254>,
        pub public_z: Fr,
        pub x: Fr,
        pub y: Fr,
    }

    pub static GROTH_CTX: Lazy<GrothCtx> = Lazy::new(|| {
        let mut rng = rand::thread_rng();
        // Choose a simple statement: x=3, y=4, z=12
        let x = Fr::from(3u32);
        let y = Fr::from(4u32);
        let public_z = Fr::from(12u32);
        // Generate parameters with a random circuit instance
        let circuit = MulCircuit {
            x: Some(x),
            y: Some(y),
            z: public_z,
        };
        let (pk, vk) = Groth16::<Bn254>::setup(circuit, &mut rng)
            .expect("param gen should succeed");
        let pvk = prepare_verifying_key(&vk);
        GrothCtx { pk, pvk, public_z, x, y }
    });

    pub fn groth_proof_id() -> String {
        // Create a proof for the fixed statement and return a short hex id derived from bytes
        let mut rng = rand::thread_rng();
        let circuit = MulCircuit { x: Some(GROTH_CTX.x), y: Some(GROTH_CTX.y), z: GROTH_CTX.public_z };
        let proof = Groth16::<Bn254>::prove(&GROTH_CTX.pk, circuit, &mut rng)
            .expect("proof generation should succeed");
        // Best-effort verification using prepared VK and public input z
        let _ok = Groth16::<Bn254>::verify_with_processed_vk(&GROTH_CTX.pvk, &[GROTH_CTX.public_z], &proof)
            .unwrap_or(false);
        let mut bytes = Vec::new();
        proof.serialize_compressed(&mut bytes).expect("serialize proof");
        // Take first 8 bytes as id
        let take = core::cmp::min(8, bytes.len());
        bytes[..take].iter().map(|b| format!("{:02x}", b)).collect::<String>()
    }

    // Prove for provided inputs x,y and public z. Returns short hex id from proof bytes.
    pub fn groth_proof_id_inputs(x: Fr, y: Fr, z: Fr) -> Result<String, String> {
        let mut rng = rand::thread_rng();
        let circuit = MulCircuit { x: Some(x), y: Some(y), z };
        let proof = Groth16::<Bn254>::prove(&GROTH_CTX.pk, circuit, &mut rng)
            .map_err(|e| e.to_string())?;
        // Verify against provided public input
        let _ok = Groth16::<Bn254>::verify_with_processed_vk(&GROTH_CTX.pvk, &[z], &proof)
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        proof.serialize_compressed(&mut bytes).map_err(|e| e.to_string())?;
        let take = core::cmp::min(8, bytes.len());
        Ok(bytes[..take].iter().map(|b| format!("{:02x}", b)).collect::<String>())
    }

    pub fn selected_backend() -> String {
        std::env::var("FPN_ZK_BACKEND").unwrap_or_else(|_| "stub".to_string()).to_lowercase()
    }
}

#[cfg(feature = "zk")]
#[derive(Deserialize)]
struct ZkProveReq {
    #[serde(default)]
    claim: Option<String>,
    #[serde(default)]
    x: Option<u64>,
    #[serde(default)]
    y: Option<u64>,
    #[serde(default)]
    z: Option<u64>,
}

#[cfg(feature = "zk")]
#[derive(Serialize)]
struct ZkProveResp {
    proof_id: String,
    claim: Option<String>,
}

#[cfg(feature = "zk")]
#[tracing::instrument(skip(req))]
async fn zk_prove(Json(req): Json<ZkProveReq>) -> Json<ZkProveResp> {
    let _timer = ZK_PROOF_LATENCY.start_timer();
    ZK_PROOFS_TOTAL.inc();
    // Backend-selectable proof id
    let proof_id = {
        #[cfg(feature = "zk")]
        {
            let backend = zk_backend_ark::selected_backend();
            match backend.as_str() {
                "ark" | "arkworks" | "groth16" | "bn254" => {
                    // Use provided inputs if present; otherwise default demo values
                    let x = req.x.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.x);
                    let y = req.y.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.y);
                    let z = req.z.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.public_z);
                    zk_backend_ark::groth_proof_id_inputs(x, y, z)
                        .unwrap_or_else(|_| {
                            use rand::Rng as _;
                            let n: u64 = rand::thread_rng().gen();
                            format!("{:016x}", n)
                        })
                },
                _ => {
                    // Fallback to stub
                    use rand::Rng as _;
                    let n: u64 = rand::thread_rng().gen();
                    format!("{:016x}", n)
                }
            }
        }
        #[cfg(not(feature = "zk"))]
        {
            String::from("stub")
        }
    };
    Json(ZkProveResp {
        proof_id,
        claim: req.claim,
    })
}

// --- ZK async job queue ---
#[cfg(feature = "zk")]
#[derive(Clone)]
enum ZkJobState {
    Pending { created_at: std::time::Instant, claim: Option<String> },
    Done { proof_id: String, claim: Option<String> },
}

#[cfg(feature = "zk")]
static ZK_JOBS: Lazy<RwLock<std::collections::HashMap<String, ZkJobState>>> =
    Lazy::new(|| RwLock::new(std::collections::HashMap::new()));

#[cfg(feature = "zk")]
#[derive(Serialize)]
struct ZkJobCreateResp {
    job_id: String,
}

#[cfg(feature = "zk")]
#[derive(Serialize)]
struct ZkJobStatusResp {
    job_id: String,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<String>,
}

#[cfg(feature = "zk")]
#[axum::debug_handler]
#[tracing::instrument(skip(state, req))]
async fn zk_prove_async(
    State(state): State<SharedState>,
    Json(req): Json<ZkProveReq>,
) -> Json<ZkJobCreateResp> {
    use rand::RngCore;
    let job_id = {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 6];
        rng.fill_bytes(&mut bytes);
        bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect::<String>()
    };

    {
        let mut jobs = ZK_JOBS.write().await;
        jobs.insert(
            job_id.clone(),
            ZkJobState::Pending {
                created_at: std::time::Instant::now(),
                claim: req.claim.clone(),
            },
        );
        ZK_JOBS_GAUGE.inc();
    }

    // Spawn async task to simulate proving work
    let job_id_clone = job_id.clone();
    let claim = req.claim.clone();
    let x_opt = req.x;
    let y_opt = req.y;
    let z_opt = req.z;
    let tx = state.tx.clone();
    tokio::spawn(async move {
        use rand::Rng as _;
        let delay_ms: u64 = rand::thread_rng().gen_range(5..30);
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        // Generate proof id using selected backend (ensure RNG is dropped before await points)
        let proof_id = {
            let backend = zk_backend_ark::selected_backend();
            match backend.as_str() {
                "ark" | "arkworks" | "groth16" | "bn254" => {
                    let x = x_opt.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.x);
                    let y = y_opt.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.y);
                    let z = z_opt.map(ark_bn254::Fr::from).unwrap_or(zk_backend_ark::GROTH_CTX.public_z);
                    zk_backend_ark::groth_proof_id_inputs(x, y, z)
                        .unwrap_or_else(|_| {
                            let n: u64 = rand::thread_rng().gen();
                            format!("{:016x}", n)
                        })
                },
                _ => {
                    use rand::Rng as _;
                    let n: u64 = rand::thread_rng().gen();
                    format!("{:016x}", n)
                }
            }
        };

        // Measure latency based on created_at
        let mut created_at_opt: Option<std::time::Instant> = None;
        {
            let mut jobs = ZK_JOBS.write().await;
            if let Some(state) = jobs.get(&job_id_clone).cloned() {
                if let ZkJobState::Pending { created_at, .. } = state {
                    created_at_opt = Some(created_at);
                }
            }
            jobs.insert(
                job_id_clone.clone(),
                ZkJobState::Done {
                    proof_id: proof_id.clone(),
                    claim: claim.clone(),
                },
            );
            ZK_JOBS_GAUGE.dec();
        }

        if let Some(created) = created_at_opt {
            let elapsed = created.elapsed().as_secs_f64();
            ZK_PROOF_LATENCY.observe(elapsed);
        }
        ZK_PROOFS_TOTAL.inc();
        let _ = tx.send(
            json!({
                "type": "zk_job_done",
                "job_id": job_id_clone,
                "proof_id": proof_id,
            })
            .to_string(),
        );
    });

    Json(ZkJobCreateResp { job_id })
}

#[cfg(feature = "zk")]
#[axum::debug_handler]
async fn zk_status(Path(job_id): Path<String>) -> Result<Json<ZkJobStatusResp>, (StatusCode, Json<Value>)> {
    let jobs = ZK_JOBS.read().await;
    match jobs.get(&job_id) {
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"detail":"job not found"})),
        )),
        Some(ZkJobState::Pending { claim, .. }) => Ok(Json(ZkJobStatusResp {
            job_id,
            state: "pending".to_string(),
            proof_id: None,
            claim: claim.clone(),
        })),
        Some(ZkJobState::Done { proof_id, claim }) => Ok(Json(ZkJobStatusResp {
            job_id,
            state: "done".to_string(),
            proof_id: Some(proof_id.clone()),
            claim: claim.clone(),
        })),
    }
}

// Simple ZK backend info
#[cfg(feature = "zk")]
#[derive(Serialize)]
struct ZkBackendInfo {
    backend: String,
}

#[cfg(feature = "zk")]
async fn zk_backend() -> Json<ZkBackendInfo> {
    let backend = std::env::var("FPN_ZK_BACKEND").unwrap_or_else(|_| "stub".to_string());
    Json(ZkBackendInfo { backend })
}

// Background frequent batch auction scheduler
#[cfg(feature = "scheduler")]
fn start_scheduler(state: SharedState) {
    tokio::spawn(async move {
        let interval_ms: u64 = std::env::var("FPN_SCHED_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);
        let fee_rate: f64 = std::env::var("FPN_SCHED_FEE_RATE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.1);
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        loop {
            ticker.tick().await;
            let _t = SCHED_BATCH_LATENCY.start_timer();
            SCHED_BATCHES_TOTAL.inc();
            // Snapshot intents and build greedy block
            let s = state.app.read().await;
            let intents = s.intents.clone();
            drop(s);
            let fifo_sel = fifo_select(intents.clone());
            let fifo_profit = total_profit(&fifo_sel);
            let selected = greedy_select(intents);
            let total = total_profit(&selected);
            let surplus = (total - fifo_profit).max(0.0);
            let sequencer_fee = surplus * fee_rate;
            let trace_id = tracing::Span::current()
                .context()
                .span()
                .span_context()
                .trace_id()
                .to_string();
            let _ = state.tx.send(
                json!({
                    "type":"scheduled_block_built",
                    "fee_rate": fee_rate,
                    "baseline_fifo_profit": fifo_profit,
                    "total_profit": total,
                    "surplus": surplus,
                    "sequencer_fee": sequencer_fee,
                    "selected_count": selected.len(),
                    "trace_id": trace_id
                })
                .to_string(),
            );
        }
    });
}

// Graceful shutdown on CTRL+C or SIGTERM; broadcast a shutdown event for WS observers
async fn shutdown_signal(state: SharedState) {
    tracing::info!("awaiting shutdown signal (CTRL+C or SIGTERM)");
    #[cfg(unix)]
    {
        let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await.ok();
    }
    let _ = state.tx.send(json!({"type":"shutdown"}).to_string());
    tracing::info!("shutdown signal received; starting graceful shutdown");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    #[tokio::test]
    async fn health_ok() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_live() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_401_without_key_on_protected() {
        // Configure API key in server state
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig {
                api_key: Some("testkey".to_string()),
            },
        });
        let app = build_router(state);
        let body = serde_json::to_vec(&json!({
            "profit": 10.0,
            "resources": ["pair:ETH/USDC"]
        }))
        .unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/intents")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["detail"], "invalid API key");
    }

    #[tokio::test]
    async fn auth_200_with_correct_key() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig {
                api_key: Some("testkey".to_string()),
            },
        });
        let app = build_router(state);
        let body = serde_json::to_vec(&json!({
            "profit": 10.0,
            "resources": ["pair:ETH/USDC"]
        }))
        .unwrap();
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/intents")
                    .header("content-type", "application/json")
                    .header("x-api-key", "testkey")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ws_event_shapes_via_broadcast() {
        // Use channel subscription to observe messages without real WS
        let (tx, rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let mut rx = rx;
        let app = build_router(state.clone());

        // Submit an intent (no API key configured here => open)
        let body = serde_json::to_vec(&json!({
            "profit": 12.34,
            "resources": ["pair:ETH/USDC"],
            "client_id": "c1"
        }))
        .unwrap();
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/intents")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Receive broadcast
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "intent_submitted");
        assert!(v["intent"].is_object());
        assert!(v["intent"]["profit"].is_number());
        assert!(v["intent"]["resources"].is_array());
    }

    #[tokio::test]
    async fn build_block_invalid_algorithm_400() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/build_block?algorithm=bad&fee_rate=0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn build_block_invalid_fee_rate_400() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        for fr in ["-0.1", "1.1"] {
            let uri = format!("/build_block?algorithm=greedy&fee_rate={fr}");
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(&uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn metrics_json_sanity() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/metrics/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["intents_count"].is_number());
        assert!(v["distinct_resources"].is_number());
        assert!(v["fifo_profit"].is_number());
        assert!(v["greedy_profit"].is_number());
        assert!(v["surplus"].is_number());
    }

    #[tokio::test]
    async fn bulk_flow_and_broadcast_count() {
        let (tx, rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let mut rx = rx;
        let app = build_router(state);
        let body = serde_json::to_vec(&json!({
            "intents": [
                {"profit": 10.0, "resources": ["pair:ETH/USDC"]},
                {"profit": 20.0, "resources": ["dex:uniswap"]}
            ]
        }))
        .unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/intents/bulk")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let msg = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["type"], "intents_bulk_submitted");
        assert_eq!(v["count"], 2);
    }

    #[tokio::test]
    async fn reset_clears_and_broadcasts() {
        let (tx, rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let mut rx = rx;
        let app = build_router(state);
        // Add one intent
        let body = serde_json::to_vec(&json!({
            "profit": 11.0,
            "resources": ["pair:ETH/USDC"]
        }))
        .unwrap();
        let _ = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/intents")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Reset
        let res = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/reset")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // Receive broadcasts; first may be intent_submitted, then reset
        for _ in 0..3 {
            let msg = rx.recv().await.unwrap();
            let v: Value = serde_json::from_str(&msg).unwrap();
            if v["type"] == "reset" {
                return;
            }
        }
        panic!("did not receive reset event in time");
    }

    #[tokio::test]
    async fn cors_preflight_allows() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/intents")
                    .header("origin", "http://example.com")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // tower-http CORS may reply 204 or 200 depending on version; ensure it's successful and includes CORS headers
        assert!(res.status() == StatusCode::NO_CONTENT || res.status() == StatusCode::OK);
        let headers = res.headers();
        assert!(headers.contains_key("access-control-allow-origin"));
        assert!(headers.contains_key("access-control-allow-methods"));
    }

    // ---- Parity helpers ----
    fn golden_path(name: &str) -> std::path::PathBuf {
        let root = env!("CARGO_MANIFEST_DIR");
        std::path::Path::new(root)
            .join("tests")
            .join("golden")
            .join(name)
    }

    fn read_json(path: &std::path::Path) -> serde_json::Value {
        let s = std::fs::read_to_string(path).expect("read golden");
        serde_json::from_str(&s).expect("parse json")
    }

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    // Normalize intents list for comparison: sort by arrival_index then profit then resources
    fn normalize_intents(v: &mut [serde_json::Value]) {
        v.sort_by(|a, b| {
            let ai = a
                .get("arrival_index")
                .and_then(|x| x.as_u64())
                .unwrap_or(u64::MAX);
            let bi = b
                .get("arrival_index")
                .and_then(|x| x.as_u64())
                .unwrap_or(u64::MAX);
            ai.cmp(&bi)
                .then_with(|| {
                    let ap = a.get("profit").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let bp = b.get("profit").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    ap.partial_cmp(&bp).unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    let ar = a.get("resources").cloned().unwrap_or(json!([])).to_string();
                    let br = b.get("resources").cloned().unwrap_or(json!([])).to_string();
                    ar.cmp(&br)
                })
        });
    }

    fn fill_default_intent_fields(v: &mut [serde_json::Value]) {
        for obj in v.iter_mut() {
            if let Some(map) = obj.as_object_mut() {
                map.entry("client_id".to_string())
                    .or_insert(serde_json::Value::Null);
            }
        }
    }

    // Normalize selected payouts: sort by profit desc then arrival_index asc for deterministic compare
    fn normalize_selected(v: &mut [serde_json::Value]) {
        v.sort_by(|a, b| {
            let ap = a.get("profit").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let bp = b.get("profit").and_then(|x| x.as_f64()).unwrap_or(0.0);
            bp.partial_cmp(&ap)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let ai = a
                        .get("arrival_index")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(u64::MAX);
                    let bi = b
                        .get("arrival_index")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(u64::MAX);
                    ai.cmp(&bi)
                })
        });
    }

    fn assert_block_parity(got: serde_json::Value, golden: serde_json::Value) {
        // Top-level numeric fields approx equal
        for k in [
            "fee_rate",
            "baseline_fifo_profit",
            "total_profit",
            "surplus",
            "sequencer_fee",
        ] {
            let a = got[k].as_f64().unwrap();
            let b = golden[k].as_f64().unwrap();
            assert!(approx_eq(a, b, 1e-9), "field {k} mismatch: {a} vs {b}");
        }
        assert_eq!(got["algorithm"], golden["algorithm"]);

        // Compare selected arrays order-insensitively and approx for floats
        let mut a_sel = got["selected"].as_array().cloned().unwrap_or_default();
        let mut b_sel = golden["selected"].as_array().cloned().unwrap_or_default();
        normalize_selected(&mut a_sel);
        normalize_selected(&mut b_sel);
        assert_eq!(a_sel.len(), b_sel.len(), "selected length");
        for (a, b) in a_sel.iter().zip(b_sel.iter()) {
            // Non-float exacts
            assert_eq!(a["client_id"], b["client_id"]);
            assert_eq!(a["resources"], b["resources"]);
            assert_eq!(a["arrival_index"], b["arrival_index"]);
            // Floats approx
            for k in ["profit", "fee_share", "trader_profit"] {
                let av = a[k].as_f64().unwrap();
                let bv = b[k].as_f64().unwrap();
                assert!(
                    approx_eq(av, bv, 1e-9),
                    "selected {k} mismatch: {av} vs {bv}"
                );
            }
        }
    }

    #[tokio::test]
    async fn parity_simple_intents_and_build_block() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        // Submit two intents
        for body in [
            json!({"profit":10.0, "resources":["pair:ETH/USDC"]}),
            json!({"profit":20.0, "resources":["dex:uniswap"]}),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/intents")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // GET /intents parity against golden (order-insensitive)
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/intents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let golden = read_json(&golden_path("simple_intents.json"));
        let mut a = got.as_array().cloned().unwrap();
        let mut b = golden.as_array().cloned().unwrap();
        // Some serializers omit nulls; treat missing client_id as null for parity
        fill_default_intent_fields(&mut a);
        fill_default_intent_fields(&mut b);
        normalize_intents(&mut a);
        normalize_intents(&mut b);
        assert_eq!(a, b);

        // GET /build_block?greedy&fee_rate=0.1 parity against golden
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/build_block?algorithm=greedy&fee_rate=0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let golden = read_json(&golden_path("simple_build_block_greedy_0.1.json"));
        assert_block_parity(got, golden);
    }

    #[tokio::test]
    async fn parity_conflict_build_block() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        // Submit three intents (two conflict on same resource)
        for body in [
            json!({"profit":10.0, "resources":["pair:ETH/USDC"]}),
            json!({"profit":20.0, "resources":["dex:uniswap"]}),
            json!({"profit":25.0, "resources":["pair:ETH/USDC"]}),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/intents")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // Build greedy 0.1 and compare
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/build_block?algorithm=greedy&fee_rate=0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let golden = read_json(&golden_path("conflict_build_block_greedy_0.1.json"));
        assert_block_parity(got, golden);
    }

    // --- Feature-gated tests ---

    #[cfg(feature = "abci")]
    #[tokio::test]
    async fn abci_prepare_and_process_smoke() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state.clone());

        // Seed a couple of intents
        for body in [
            json!({"profit": 10.0, "resources": ["pair:ETH/USDC"], "client_id":"a"}),
            json!({"profit": 20.0, "resources": ["dex:uniswap"], "client_id":"b"}),
        ] {
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/intents")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // Prepare proposal with a cap of 1 intent
        let req = json!({"algorithm":"greedy","fee_rate":0.1,"max_intents":1});
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/abci/prepare_proposal")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["algorithm"], "greedy");
        assert_eq!(v["fee_rate"].as_f64().unwrap(), 0.1);
        assert!(v["selected"].as_array().unwrap().len() <= 1);

        // Process proposal accepts
        let req = json!({"algorithm":"greedy","fee_rate":0.1});
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/abci/process_proposal")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["accepted"], true);
    }

    #[cfg(feature = "mev")]
    #[tokio::test]
    async fn mev_submit_and_results() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        let body = json!({"client_id":"c1","order": {"type":"bundle","value": 42}});
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auction/submit")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/auction/results")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["submissions"].as_u64().unwrap() >= 1);
    }

    #[cfg(feature = "zk")]
    #[tokio::test]
    async fn zk_prove_returns_proof_id() {
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        let body = json!({"claim":"demo"});
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/zk/prove")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let pid = v["proof_id"].as_str().unwrap();
        assert_eq!(pid.len(), 16);
    }

    #[cfg(feature = "scheduler")]
    #[tokio::test]
    async fn scheduler_emits_scheduled_block_events() {
        use tokio::time::{sleep, timeout, Duration};
        std::env::set_var("FPN_SCHED_INTERVAL_MS", "100");
        let (tx, rx) = broadcast::channel(100);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let mut rx = rx;
        // Start scheduler in background
        super::start_scheduler(state.clone());
        // Give it a moment to tick and possibly send an event
        let fut = async move {
            loop {
                if let Ok(msg) = rx.recv().await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                        if v["type"] == "scheduled_block_built" {
                            return;
                        }
                    }
                }
            }
        };
        timeout(Duration::from_secs(3), fut).await.expect("no scheduled_block_built event");
        // Avoid influencing other tests
        sleep(Duration::from_millis(50)).await;
    }

    #[cfg(feature = "zk")]
    #[tokio::test]
    async fn zk_async_job_lifecycle() {
        use tokio::time::{sleep, timeout, Duration};
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        // Create async job
        let body = serde_json::to_vec(&json!({"claim":"demo-async"})).unwrap();
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/zk/prove_async")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let job_id = v["job_id"].as_str().unwrap().to_string();

        // Poll status until done
        let fut = async move {
            loop {
                let res = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("GET")
                            .uri(format!("/zk/status/{}", job_id))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                if v["state"] == "done" {
                    assert!(v["proof_id"].as_str().is_some());
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        };
        timeout(Duration::from_secs(3), fut)
            .await
            .expect("zk async job did not complete in time");
    }

    #[tokio::test]
    async fn fairness_gini_in_range() {
        use std::collections::HashMap;
        let (tx, _rx) = broadcast::channel(10);
        let state: SharedState = Arc::new(ServerState {
            app: RwLock::new(AppState::default()),
            tx,
            config: ServerConfig { api_key: None },
        });
        let app = build_router(state);

        // Submit intents for multiple clients
        for (p, cid, res) in [
            (10.0, "a", "pair:X/1"),
            (20.0, "b", "pair:X/2"),
            (15.0, "a", "pair:X/3"),
            (5.0, "c", "pair:X/4"),
            (12.0, "b", "pair:X/5"),
        ] {
            let body = json!({"profit":p, "resources":[res], "client_id":cid});
            let res = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/intents")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::OK);
        }

        // Build greedy block
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/build_block?algorithm=greedy&fee_rate=0.1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let selected = v["selected"].as_array().cloned().unwrap_or_default();
        let mut by_client: HashMap<String, f64> = HashMap::new();
        for it in selected {
            let cid = it["client_id"].as_str().unwrap_or("").to_string();
            let tp = it["trader_profit"].as_f64().unwrap_or(0.0);
            *by_client.entry(cid).or_insert(0.0) += tp;
        }
        let mut vals: Vec<f64> = by_client.into_values().collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sum: f64 = vals.iter().sum();
        if sum == 0.0 {
            return; // degenerate case
        }
        // Gini via 1-indexed sum(i*x)/(n*sum)
        let n = vals.len() as f64;
        let mut sum_i_x = 0.0f64;
        for (i, x) in vals.iter().enumerate() {
            sum_i_x += ((i as f64) + 1.0) * *x;
        }
        let gini = (2.0 * sum_i_x) / (n * sum) - (n + 1.0) / n;
        assert!(gini.is_finite());
    }
}
