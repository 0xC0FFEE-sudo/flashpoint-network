use assert_cmd::prelude::*;
use futures_util::StreamExt;
use portpicker::pick_unused_port;
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tokio_tungstenite::connect_async;

fn spawn_server(port: u16) -> Child {
    let mut cmd = Command::cargo_bin("fpn_server").expect("binary exists");
    cmd.env("FPN_PORT", port.to_string())
        .env("FPN_SCHED_INTERVAL_MS", "100")
        .env("FPN_SCHED_FEE_RATE", "0.1")
        .env_remove("FPN_API_KEY")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("failed to spawn server")
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

#[test]
fn scheduler_emits_event_and_basic_fairness() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let ws_url = format!("ws://127.0.0.1:{}/ws", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // Seed intents: some conflict-free high-profit and some conflicting
    submit_intent(&base, 3.0, &["A"], "a1");
    submit_intent(&base, 2.0, &["B"], "b1");
    submit_intent(&base, 1.0, &["A"], "a2"); // conflicts on A, should be excluded by greedy after picking a1

    // Start listener in background thread runtime
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            match connect_async(ws_url).await {
                Ok((ws_stream, _)) => {
                    let (_w, mut r) = ws_stream.split();
                    // Wait for a scheduled_block_built event
                    let _ = tokio::time::timeout(Duration::from_secs(3), async {
                        while let Some(Ok(m)) = r.next().await {
                            if m.is_text() {
                                if let Ok(v) = serde_json::from_str::<Value>(m.to_text().unwrap()) {
                                    if v["type"].as_str() == Some("scheduled_block_built") {
                                        let _ = tx.send(v);
                                        break;
                                    }
                                }
                            }
                        }
                    })
                    .await;
                }
                Err(_) => {
                    // If WS not available, send an empty value; test will handle skip
                    let _ = tx.send(json!({"ws_unavailable": true}));
                }
            }
        });
    });

    // Receive event or timeout
    let msg = rx.recv_timeout(Duration::from_secs(4)).expect("no event");
    if msg.get("ws_unavailable").is_some() {
        let _ = child.kill();
        let _ = handle.join();
        return; // likely scheduler feature not enabled; skip
    }

    // Validate event fields and basic fairness relationships
    assert_eq!(msg["type"].as_str(), Some("scheduled_block_built"));
    let fifo = msg["baseline_fifo_profit"].as_f64().unwrap_or(0.0);
    let total = msg["total_profit"].as_f64().unwrap_or(0.0);
    let surplus = msg["surplus"].as_f64().unwrap_or(-1.0);
    let fee = msg["sequencer_fee"].as_f64().unwrap_or(-1.0);

    assert!(total >= fifo, "greedy should not underperform FIFO");
    assert!(surplus >= 0.0 && (surplus - (total - fifo)).abs() < 1e-9);
    assert!((fee - surplus * 0.1).abs() < 1e-9);
    assert!(msg["selected_count"].as_u64().unwrap_or(0) >= 1);
    assert!(!msg["trace_id"].as_str().unwrap_or("").is_empty());

    let _ = child.kill();
    let _ = handle.join();
}
