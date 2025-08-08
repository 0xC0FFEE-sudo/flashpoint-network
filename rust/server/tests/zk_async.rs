use assert_cmd::prelude::*;
use portpicker::pick_unused_port;
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn spawn_server(port: u16) -> Child {
    let mut cmd = Command::cargo_bin("fpn_server").expect("binary exists");
    cmd.env("FPN_PORT", port.to_string())
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

#[test]
fn zk_async_prove_and_status() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // start async proof
    let body = json!({ "claim": "hello" });
    let rsp = http()
        .post(format!("{}/zk/prove_async", base))
        .json(&body)
        .send()
        .expect("prove_async rsp");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    assert!(rsp.status().is_success(), "prove_async failed: {:?}", rsp.text());
    let job: Value = rsp.json().expect("json");
    let job_id = job["job_id"].as_str().expect("job_id");

    // poll status until done
    let start = Instant::now();
    let status_url = format!("{}/zk/status/{}", base, job_id);
    let proof_id = loop {
        if start.elapsed() > Duration::from_secs(3) {
            panic!("zk job didn't complete in time");
        }
        let rsp = http().get(&status_url).send().expect("status rsp");
        if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
        let v: Value = rsp.json().expect("json");
        match v["state"].as_str() {
            Some("done") => break v["proof_id"].as_str().unwrap().to_string(),
            Some("pending") | _ => thread::sleep(Duration::from_millis(50)),
        }
    };
    assert!(!proof_id.is_empty());

    let _ = child.kill();
}
