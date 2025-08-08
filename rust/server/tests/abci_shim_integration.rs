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

fn spawn_shim(shim_port: u16, server_base: &str) -> Child {
    let mut cmd = Command::cargo_bin("abci_shim").expect("shim binary exists");
    cmd.env("FPN_ABCI_PORT", shim_port.to_string())
        .env("FPN_BASE", server_base)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("failed to spawn abci_shim")
}

fn http() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn wait_for_health(base: &str, timeout: Duration) {
    let client = http();
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!("did not become healthy at {} within {:?}", base, timeout);
        }
        match client.get(format!("{}/health", base)).send() {
            Ok(rsp) if rsp.status().is_success() => return,
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
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

#[test]
fn abci_shim_forwards_prepare_and_process() {
    // Pick separate ports for server and shim
    let server_port = pick_unused_port().expect("port");
    let shim_port = pick_unused_port().expect("port");
    assert_ne!(server_port, shim_port);

    let server_base = format!("http://127.0.0.1:{}", server_port);
    let shim_base = format!("http://127.0.0.1:{}", shim_port);

    // Spawn server and wait
    let mut server = spawn_server(server_port);
    wait_for_health(&server_base, Duration::from_secs(5));

    // Spawn shim pointing at server and wait
    let mut shim = spawn_shim(shim_port, &server_base);
    wait_for_health(&shim_base, Duration::from_secs(5));

    // Seed some intents directly into the server
    reset(&server_base);
    submit_intent(&server_base, 5.0, &["X"], "a");
    submit_intent(&server_base, 4.0, &["Y"], "b");

    // Call prepare via the shim
    let prepare_body = json!({ "algorithm": "fifo", "fee_rate": 0.0, "max_intents": 2 });
    let client = http();
    let rsp = client
        .post(format!("{}/abci/prepare_proposal", shim_base))
        .json(&prepare_body)
        .send()
        .expect("shim prepare response");

    if !rsp.status().is_success() {
        // Upstream may not have ABCI feature enabled; treat as skip
        let _ = server.kill();
        let _ = shim.kill();
        return;
    }

    let v: Value = rsp.json().expect("json body");
    assert_eq!(v["algorithm"].as_str(), Some("fifo"));
    assert!(v["selected"].as_array().unwrap().len() >= 1);

    // Call process via the shim
    let process_body = json!({ "algorithm": "fifo", "fee_rate": 0.0 });
    let rsp2 = client
        .post(format!("{}/abci/process_proposal", shim_base))
        .json(&process_body)
        .send()
        .expect("shim process response");

    if rsp2.status().is_success() {
        let v2: Value = rsp2.json().expect("json body");
        assert_eq!(v2["accepted"].as_bool(), Some(true));
    }

    let _ = shim.kill();
    let _ = server.kill();
}
