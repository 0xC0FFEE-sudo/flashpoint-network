use once_cell::sync::Lazy;
use reqwest::blocking::{Client, Response};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, Criterion};

struct BenchEnv {
    _child: Child,
    client: Client,
    base: String,
}

static ENV: Lazy<BenchEnv> = Lazy::new(|| spawn_server_and_client(18081));

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
        eprintln!("WARNING: {} not found. Ensure the server binary is built in release mode before running benches.", bin.display());
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

    // Reset and seed state before the bench
    let _ = client.delete(format!("{}/reset", base)).send();
    // Seed via /seed for speed
    let _ = client
        .post(format!("{}/seed", base))
        .json(&serde_json::json!({"count": 500}))
        .send();

    BenchEnv {
        _child: child,
        client,
        base,
    }
}

fn bench_build_block(c: &mut Criterion) {
    // Warm-up
    let warm = ENV
        .client
        .get(format!("{}/build_block", ENV.base))
        .query(&[("algorithm", "greedy"), ("fee_rate", "0.1")])
        .send()
        .expect("warmup");
    assert!(warm.status().is_success());

    c.bench_function("build_block", |b| {
        b.iter(|| {
            let r: Response = ENV
                .client
                .get(format!("{}/build_block", ENV.base))
                .query(&[("algorithm", "greedy"), ("fee_rate", "0.1")])
                .send()
                .expect("send");
            assert!(r.status().is_success());
        })
    });
}

criterion_group!(benches, bench_build_block);
criterion_main!(benches);
