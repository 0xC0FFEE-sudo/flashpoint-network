use once_cell::sync::Lazy;
use reqwest::blocking::{Client, Response};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

struct BenchEnv {
    _child: Child,
    client: Client,
    base: String,
}

static ENV: Lazy<BenchEnv> = Lazy::new(|| spawn_server_and_client(18083));

fn server_bin_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target");
    p.push("release");
    if cfg!(windows) {
        p.push("fpn_server.exe");
    } else {
        p.push("fpn_server");
    }
    p
}

fn wait_for_health(client: &Client, base: &str, timeout: Duration) {
    let start = Instant::now();
    loop {
        if start.elapsed() > timeout {
            panic!(
                "server did not become healthy at {} within {:?}",
                base, timeout
            );
        }
        match client.get(format!("{}/health", base)).send() {
            Ok(resp) if resp.status().is_success() => return,
            _ => sleep(Duration::from_millis(50)),
        }
    }
}

fn spawn_server_and_client(port: u16) -> BenchEnv {
    let bin = server_bin_path();
    if !bin.exists() {
        eprintln!("WARNING: {} not found. Build release binary first.", bin.display());
    }
    let mut cmd = Command::new(bin);
    cmd.env("RUST_LOG", "warn")
        .env("FPN_PORT", port.to_string())
        .env("FPN_CONCURRENCY", "65535")
        .env("FPN_BUFFER_SIZE", "65535")
        .env("FPN_REQS_PER_SEC", "18446744073709551615")
        .env("FPN_WS_CHANNEL_CAP", "1000")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn server");

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{}", port);
    wait_for_health(&client, &base, Duration::from_secs(5));

    // Reset and seed state before the bench: create mixed client_ids
    let _ = client.delete(format!("{}/reset", base)).send();
    for i in 0..500u32 {
        let client_id = format!("cli-{}", i % 16);
        let body = serde_json::json!({
            "profit": (10.0 + (i % 100) as f64) / 2.0,
            "resources": [format!("pair:ASSET{}", i % 128)],
            "client_id": client_id,
        });
        let _ = client
            .post(format!("{}/intents", base))
            .json(&body)
            .send();
    }

    BenchEnv { _child: child, client, base }
}

fn gini(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len() as f64;
    let sum: f64 = v.iter().sum();
    if sum == 0.0 {
        return 0.0;
    }
    let mut sum_i_x = 0.0f64;
    for (i, x) in v.iter().enumerate() {
        let i1 = (i as f64) + 1.0; // 1-indexed
        sum_i_x += i1 * *x;
    }
    let g = (2.0 * sum_i_x) / (n * sum) - (n + 1.0) / n;
    g.clamp(0.0, 1.0)
}

fn bench_fairness(c: &mut Criterion) {
    // Build once to establish selection and payouts
    let r: Response = ENV
        .client
        .get(format!("{}/build_block", ENV.base))
        .query(&[("algorithm", "greedy"), ("fee_rate", "0.1")])
        .send()
        .expect("send");
    assert!(r.status().is_success());
    let data: serde_json::Value = r.json().expect("json");
    let selected = data["selected"].as_array().cloned().unwrap_or_default();
    let mut by_client: HashMap<String, f64> = HashMap::new();
    for it in selected {
        let cid = it["client_id"].as_str().unwrap_or("").to_string();
        let profit = it["trader_profit"].as_f64().unwrap_or(0.0);
        *by_client.entry(cid).or_insert(0.0) += profit;
    }
    let totals: Vec<f64> = by_client.into_values().collect();

    c.bench_function("fairness_gini_trader_profit_by_client", |b| {
        b.iter(|| {
            let g = gini(&totals);
            black_box(g);
        })
    });
}

criterion_group!(benches, bench_fairness);
criterion_main!(benches);


