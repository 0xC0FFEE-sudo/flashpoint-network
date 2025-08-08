use assert_cmd::prelude::*;
use portpicker::pick_unused_port;
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn spawn_server(port: u16) -> Child {
    let mut cmd = Command::cargo_bin("fpn_server").expect("binary exists");
    cmd.env("FPN_PORT", port.to_string())
        // ensure protected endpoints are open without API key for tests
        .env_remove("FPN_API_KEY")
        // avoid external OTEL egress during tests
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("failed to spawn server")
}

async fn async_wait_for_health(base: &str, timeout: Duration) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!("server did not become healthy at {} within {:?}", base, timeout);
        }
        match client.get(format!("{}/health", base)).send().await {
            Ok(rsp) if rsp.status().is_success() => return,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

fn wait_for_health(base: &str, timeout: Duration) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!("server did not become healthy at {} within {:?}", base, timeout);
        }
        match client.get(format!("{}/health", base)).send() {
            Ok(rsp) if rsp.status().is_success() => return,
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn reset(base: &str) {
    let _ = http().delete(format!("{}/reset", base)).send();
}

fn submit_intent(base: &str, profit: f64, resources: &[&str], client_id: &str) {
    let body = json!({
        "profit": profit,
        "resources": resources,
        "client_id": client_id,
    });
    let rsp = http()
        .post(format!("{}/intents", base))
        .json(&body)
        .send()
        .expect("submit intent response");
    assert!(rsp.status().is_success(), "submit intent failed: {:?}", rsp.text());
}

// Async variants for use inside async tests to avoid blocking inside a tokio context
async fn async_reset(base: &str) {
    let client = reqwest::Client::new();
    let _ = client.delete(format!("{}/reset", base)).send().await;
}

async fn async_submit_intent(base: &str, profit: f64, resources: &[&str], client_id: &str) {
    let client = reqwest::Client::new();
    let body = json!({
        "profit": profit,
        "resources": resources,
        "client_id": client_id,
    });
    let rsp = client
        .post(format!("{}/intents", base))
        .json(&body)
        .send()
        .await
        .expect("submit intent response");
    assert!(rsp.status().is_success(), "submit intent failed: {:?}", rsp.text().await.ok());
}

fn abci_prepare(base: &str, algorithm: &str, fee_rate: f64, max_intents: Option<usize>) -> Option<Value> {
    let mut body = json!({ "algorithm": algorithm, "fee_rate": fee_rate });
    if let Some(k) = max_intents { body["max_intents"] = json!(k); }
    let rsp = http()
        .post(format!("{}/abci/prepare_proposal", base))
        .json(&body)
        .send()
        .expect("abci prepare response");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { return None; }
    assert!(rsp.status().is_success(), "prepare failed: {:?}", rsp.text());
    Some(rsp.json::<Value>().expect("json body"))
}

fn abci_process(base: &str, algorithm: &str, fee_rate: f64) -> Option<Value> {
    let body = json!({ "algorithm": algorithm, "fee_rate": fee_rate });
    let rsp = http()
        .post(format!("{}/abci/process_proposal", base))
        .json(&body)
        .send()
        .expect("abci process response");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { return None; }
    assert!(rsp.status().is_success(), "process failed: {:?}", rsp.text());
    Some(rsp.json::<Value>().expect("json body"))
}

#[test]
fn abci_prepare_fifo_truncation_and_payouts() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // Isolate state
    reset(&base);

    // Submit in order with a conflict on X for the 3rd intent
    submit_intent(&base, 10.0, &["X"], "c1"); // arrival 1
    submit_intent(&base, 8.0, &["Y"], "c2");  // arrival 2
    submit_intent(&base, 5.0, &["X"], "c3");  // arrival 3 (conflicts with c1)
    submit_intent(&base, 1.0, &["Z"], "c4"); // arrival 4 (small positive)

    // Truncate to first 3 by arrival; FIFO should pick c1 and c2 only
    let resp = match abci_prepare(&base, "fifo", 0.5, Some(3)) {
        None => { let _ = child.kill(); return; }, // feature not enabled; treat as skipped
        Some(v) => v,
    };

    // baseline equals total, surplus and sequencer_fee are zero
    let fifo_profit = resp["baseline_fifo_profit"].as_f64().unwrap();
    let total = resp["total_profit"].as_f64().unwrap();
    let surplus = resp["surplus"].as_f64().unwrap();
    let fee = resp["sequencer_fee"].as_f64().unwrap();
    assert_eq!(fifo_profit, total);
    assert_eq!(surplus, 0.0);
    assert_eq!(fee, 0.0);

    let selected = resp["selected"].as_array().unwrap();
    assert_eq!(selected.len(), 2);

    // Validate payouts: fee_share 0, trader_profit equals profit
    for it in selected.iter() {
        let p = it["profit"].as_f64().unwrap();
        let share = it["fee_share"].as_f64().unwrap();
        let trader = it["trader_profit"].as_f64().unwrap();
        assert_eq!(share, 0.0);
        assert!((trader - p.max(0.0)).abs() < 1e-9);
    }

    let _ = child.kill();
}

#[test]
fn abci_prepare_greedy_surplus_and_fee_distribution() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    reset(&base);

    // Construct a case where FIFO is worse than greedy (by arrival order)
    // i1 arrives first using X with low profit; greedy should prefer i2 (X) and i3 (Y)
    submit_intent(&base, 5.0, &["X"], "i1"); // arrival 1
    submit_intent(&base, 9.0, &["X"], "i2"); // arrival 2
    submit_intent(&base, 8.0, &["Y"], "i3"); // arrival 3

    let resp = match abci_prepare(&base, "greedy", 0.25, None) {
        None => { let _ = child.kill(); return; },
        Some(v) => v,
    };

    let fifo_profit = resp["baseline_fifo_profit"].as_f64().unwrap(); // expect 13
    let total = resp["total_profit"].as_f64().unwrap();               // expect 17
    let surplus = resp["surplus"].as_f64().unwrap();                   // expect 4
    let fee = resp["sequencer_fee"].as_f64().unwrap();                 // 1.0

    assert!((fifo_profit - 13.0).abs() < 1e-9, "fifo_profit={}", fifo_profit);
    assert!((total - 17.0).abs() < 1e-9, "total={}", total);
    assert!((surplus - 4.0).abs() < 1e-9, "surplus={}", surplus);
    assert!((fee - 1.0).abs() < 1e-9, "fee={}", fee);

    // Sum of fee_share equals sequencer_fee; distribution proportional to non-negative profits
    let selected = resp["selected"].as_array().unwrap();
    let sum_fee: f64 = selected
        .iter()
        .map(|it| it["fee_share"].as_f64().unwrap())
        .sum();
    assert!((sum_fee - fee).abs() < 1e-9, "sum_fee={} fee={}", sum_fee, fee);

    let gross: f64 = selected
        .iter()
        .map(|it| it["profit"].as_f64().unwrap().max(0.0))
        .sum();
    for it in selected {
        let p = it["profit"].as_f64().unwrap().max(0.0);
        let share = it["fee_share"].as_f64().unwrap();
        let expected = if gross > 0.0 { fee * (p / gross) } else { 0.0 };
        assert!((share - expected).abs() < 1e-9);
    }

    let _ = child.kill();
}

#[test]
fn abci_prepare_event_emission_trace_id() {
    use futures_util::StreamExt;
    use tokio_tungstenite::connect_async;
    use std::sync::mpsc;

    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}/ws", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    reset(&base);
    submit_intent(&base, 3.0, &["A"], "a");
    submit_intent(&base, 2.0, &["B"], "b");

    let (tx, rx) = mpsc::channel();

    // Spawn a background thread with its own Tokio runtime to listen for the WS event
    let ws_url_clone = ws_url.clone();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let (ws_stream, _) = connect_async(ws_url_clone).await.expect("ws connect");
            let (_write, mut read) = ws_stream.split();
            if let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(2), read.next()).await {
                if m.is_text() {
                    let v: Value = serde_json::from_str(m.to_text().unwrap()).unwrap_or(json!({}));
                    let _ = tx.send(v);
                }
            }
        });
    });

    // Trigger prepare via blocking HTTP; the listener should capture the event
    let _ = abci_prepare(&base, "fifo", 0.1, None);

    let msg = rx.recv_timeout(Duration::from_secs(3)).expect("ws event timeout");
    assert_eq!(msg["type"].as_str(), Some("abci_prepare_proposal"));
    assert!(msg["selected_count"].as_u64().unwrap_or(0) >= 1);
    let trace_id = msg["trace_id"].as_str().unwrap_or("");
    assert!(!trace_id.is_empty());

    let _ = child.kill();
    let _ = handle.join();
}

#[test]
fn abci_process_proposal_accepts_and_emits() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    let resp = match abci_process(&base, "fifo", 0.0) {
        None => { let _ = child.kill(); return; },
        Some(v) => v,
    };
    assert_eq!(resp["accepted"].as_bool(), Some(true));

    let _ = child.kill();
}
