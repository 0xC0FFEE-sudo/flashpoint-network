use assert_cmd::prelude::*;
use criterion::{criterion_group, criterion_main, Criterion};
use portpicker::pick_unused_port;
use serde_json::json;
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

fn bench_auction_book(c: &mut Criterion) {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // Seed N submissions with deterministic bids so sorting/top-k is meaningful
    let n = 500;
    for i in 0..n {
        let bid = ((i * 37) % 101) as f64 + 0.5; // pseudo-random yet deterministic
        let body = json!({
            "client_id": format!("c{}", i),
            "order": { "bid": bid }
        });
        let rsp = http()
            .post(format!("{}/auction/submit", base))
            .json(&body)
            .send()
            .expect("submit rsp");
        if rsp.status() == reqwest::StatusCode::NOT_FOUND {
            let _ = child.kill();
            return; // feature not enabled; skip bench
        }
        assert!(rsp.status().is_success());
    }

    let book_url = format!("{}/auction/book", base);
    c.bench_function("auction_book_top10", |b| {
        b.iter(|| {
            let rsp = http().get(&book_url).send().unwrap();
            assert!(rsp.status().is_success());
            // Avoid parsing heavy JSON to keep measurement focused on server
        })
    });

    let _ = child.kill();
}

criterion_group!(benches, bench_auction_book);
criterion_main!(benches);
