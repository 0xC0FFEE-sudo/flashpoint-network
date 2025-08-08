use axum::{routing::post, Json, Router};
use axum::{extract::State, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc};
use tokio::net::TcpListener;

#[derive(Clone)]
struct ShimState {
    target_base: String,
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

async fn prepare(
    State(state): State<Arc<ShimState>>,
    Json(req): Json<PrepareProposalReq>,
) -> Result<Json<BlockResponse>, (axum::http::StatusCode, Json<Value>)> {
    let url = format!("{}/abci/prepare_proposal", state.target_base);
    let resp = reqwest::Client::new()
        .post(url)
        .json(&req)
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    let status = resp.status();
    let v = resp
        .json::<BlockResponse>()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    if !status.is_success() {
        return Err((status, Json(json!({"detail":"upstream error"}))));
    }
    Ok(Json(v))
}

async fn process(
    State(state): State<Arc<ShimState>>,
    Json(req): Json<ProcessProposalReq>,
) -> Result<Json<Value>, (axum::http::StatusCode, Json<Value>)> {
    let url = format!("{}/abci/process_proposal", state.target_base);
    let resp = reqwest::Client::new()
        .post(url)
        .json(&req)
        .send()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    let status = resp.status();
    let v = resp
        .json::<Value>()
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_GATEWAY, Json(json!({"detail": e.to_string()}))))?;
    if !status.is_success() {
        return Err((status, Json(json!({"detail":"upstream error"}))));
    }
    Ok(Json(v))
}

#[tokio::main]
async fn main() {
    let target_base = std::env::var("FPN_BASE").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let port: u16 = std::env::var("FPN_ABCI_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(26658);
    let state = Arc::new(ShimState { target_base });

    let app = Router::new()
        .route("/health", get(health))
        .route("/abci/prepare_proposal", post(prepare))
        .route("/abci/process_proposal", post(process))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("abci_shim listening on {}", addr);
    let listener = TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
