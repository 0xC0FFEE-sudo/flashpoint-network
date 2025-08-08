use once_cell::sync::Lazy;
use reqwest::blocking::{Client, Response};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

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
            panic!("server not healthy {} in {:?}", base, timeout);
        }
        match client.get(format!("{}/health", base)).send() {
            Ok(resp) if resp.status().is_success() => return,
            _ => sleep(Duration::from_millis(50)),
        }
    }
}

fn spawn_server_and_client(port: u16) -> BenchEnv {
    let bin = server_bin_path();
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
    // Clear mempool
    let _ = client.delete(format!("{}/reset", &base)).send();
    BenchEnv {
        _child: child,
        client,
        base,
    }
}

fn bench_build_block_param(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_block_param");
    // Vary mempool size via /seed and fee_rate; keep algorithm fixed (greedy)
    let sizes = [100usize, 500, 2000];
    let fee_rates = [0.01_f64, 0.1, 0.5];

    for &sz in &sizes {
        // Reset and seed once per size to keep iterations focused on GET cost
        let _ = ENV.client.delete(format!("{}/reset", ENV.base)).send();
        let _ = ENV
            .client
            .post(format!("{}/seed", ENV.base))
            .json(&serde_json::json!({"count": sz as i32}))
            .send();

        for &fee in &fee_rates {
            let id = BenchmarkId::new(format!("size:{}", sz), format!("fee:{}", fee));
            group.bench_with_input(id, &fee, |b, &fee| {
                b.iter(|| {
                    let r: Response = ENV
                        .client
                        .get(format!("{}/build_block", ENV.base))
                        .query(&[("algorithm", "greedy"), ("fee_rate", &fee.to_string())])
                        .send()
                        .expect("send");
                    assert!(r.status().is_success());
                })
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_build_block_param);
criterion_main!(benches);
