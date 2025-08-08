use axum::{routing::post, Json, Router};
use axum::{extract::State, http::HeaderMap, routing::get};
use clap::Parser;
use prometheus::{register_histogram_vec, Encoder, HistogramVec, IntCounterVec, TextEncoder, register_int_counter_vec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{info, warn};

#[derive(Clone)]
struct ShimState {
    target_base: String,
    client: reqwest::Client,
    api_key: Option<String>,
    retries: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PrepareProposalReq {
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_fee_rate")]
    fee_rate: f64,
    #[serde(default)]
    max_intents: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProcessProposalReq {
    #[serde(default = "default_algorithm")]
    algorithm: String,
    #[serde(default = "default_fee_rate")]
    fee_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IntentPayout {
    profit: f64,
    fee_share: f64,
    trader_profit: f64,
    resources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arrival_index: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BlockResponse {
    algorithm: String,
    fee_rate: f64,
    baseline_fifo_profit: f64,
    total_profit: f64,
    surplus: f64,
    sequencer_fee: f64,
    selected: Vec<IntentPayout>,
}

fn default_algorithm() -> String {
    "greedy".to_string()
}
fn default_fee_rate() -> f64 {
    0.1
}

async fn health() -> &'static str {
    "ok"
}

async fn metrics() -> ([(axum::http::HeaderName, axum::http::HeaderValue); 1], String) {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buf = Vec::new();
    encoder.encode(&metric_families, &mut buf).unwrap();
    let body = String::from_utf8(buf).unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, axum::http::HeaderValue::from_static("text/plain; version=0.0.4"))],
        body,
    )
}

async fn ready(State(state): State<Arc<ShimState>>) -> axum::http::StatusCode {
    match state.client.get(format!("{}/health", state.target_base)).send().await {
        Ok(rsp) if rsp.status().is_success() => axum::http::StatusCode::OK,
        _ => axum::http::StatusCode::SERVICE_UNAVAILABLE,
    }
}

static FORWARDED_REQS: once_cell::sync::Lazy<IntCounterVec> = once_cell::sync::Lazy::new(|| {
    register_int_counter_vec!(
        "fpn_abci_shim_forwarded_total",
        "Total forwarded requests",
        &["endpoint", "status"]
    )
    .unwrap()
});

static UPSTREAM_LATENCY: once_cell::sync::Lazy<HistogramVec> = once_cell::sync::Lazy::new(|| {
    register_histogram_vec!(
        "fpn_abci_shim_upstream_seconds",
        "Upstream request latencies",
        &["endpoint"]
    )
    .unwrap()
});

fn header_traceparent(h: &HeaderMap) -> Option<String> {
    h.get("traceparent").and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

async fn post_with_retry<T: serde::Serialize>(
    state: &ShimState,
    endpoint: &str,
    body: &T,
    inbound_headers: &HeaderMap,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}/{}", state.target_base, endpoint);
    let tp = header_traceparent(inbound_headers);
    let mut attempt: u8 = 0;
    loop {
        let _timer = UPSTREAM_LATENCY.with_label_values(&[endpoint]).start_timer();
        let mut req = state.client.post(&url).json(body);
        if let Some(tpv) = &tp {
            req = req.header("traceparent", tpv);
        }
        if let Some(key) = &state.api_key {
            req = req.header("x-api-key", key);
        }
        let res = req.send().await;
        match &res {
            Ok(rsp) if rsp.status().is_success() => return res,
            Ok(rsp) if rsp.status().as_u16() == 502 || rsp.status().as_u16() == 503 => {
                // retryable
            }
            Err(_) => {
                // retryable network error
            }
            _ => return res, // non-retryable e.g. 4xx
        }
        attempt += 1;
        if attempt > state.retries {
            return res;
        }
        let backoff_ms = 10u64.saturating_mul(2u64.pow((attempt - 1) as u32));
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }
}

async fn prepare(
    State(state): State<Arc<ShimState>>,
    headers: HeaderMap,
    Json(req): Json<PrepareProposalReq>,
) -> Result<Json<BlockResponse>, (axum::http::StatusCode, Json<Value>)> {
    let resp = post_with_retry(&state, "abci/prepare_proposal", &req, &headers)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    let status = resp.status();
    let v = resp
        .json::<BlockResponse>()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    if !status.is_success() {
        FORWARDED_REQS
            .with_label_values(&["prepare_proposal", status.as_str()])
            .inc();
        return Err((status, Json(json!({"detail":"upstream error"}))));
    }
    FORWARDED_REQS
        .with_label_values(&["prepare_proposal", status.as_str()])
        .inc();
    Ok(Json(v))
}

async fn process(
    State(state): State<Arc<ShimState>>,
    headers: HeaderMap,
    Json(req): Json<ProcessProposalReq>,
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let resp = post_with_retry(&state, "abci/process_proposal", &req, &headers)
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    let status = resp.status();
    let v = resp
        .json::<Value>()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    if !status.is_success() {
        FORWARDED_REQS
            .with_label_values(&["process_proposal", status.as_str()])
            .inc();
        return Err((status, Json(json!({"detail":"upstream error"}))));
    }
    FORWARDED_REQS
        .with_label_values(&["process_proposal", status.as_str()])
        .inc();
    Ok(Json(v))
}

#[derive(Parser, Debug)]
#[command(name = "abci_shim", about = "ABCI HTTP shim for Flashpoint Network")] 
struct Args {
    /// Upstream base URL for the Rust server
    #[arg(long, env = "FPN_BASE", default_value = "http://127.0.0.1:8080")]
    base: String,

    /// Port to listen on
    #[arg(long, env = "FPN_ABCI_PORT", default_value_t = 26658)]
    port: u16,

    /// API key to forward as x-api-key
    #[arg(long, env = "FPN_ABCI_API_KEY")]
    api_key: Option<String>,

    /// Upstream timeout in milliseconds
    #[arg(long, env = "FPN_ABCI_TIMEOUT_MS", default_value_t = 2000)]
    timeout_ms: u64,

    /// Number of retries on 502/503/network errors
    #[arg(long, env = "FPN_ABCI_RETRIES", default_value_t = 2)]
    retries: u8,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    info!(base = %args.base, port = args.port, "starting abci_shim");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(args.timeout_ms))
        .pool_idle_timeout(Some(Duration::from_secs(10)))
        .build()
        .expect("reqwest client");
    let state = Arc::new(ShimState {
        target_base: args.base.clone(),
        client,
        api_key: args.api_key.clone().or_else(|| std::env::var("FPN_API_KEY").ok()),
        retries: args.retries,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/abci/prepare_proposal", post(prepare))
        .route("/abci/process_proposal", post(process))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("abci_shim listening on {}", addr);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = signal::ctrl_c().await;
            warn!("shutting down abci_shim");
        })
        .await
        .unwrap();
}
