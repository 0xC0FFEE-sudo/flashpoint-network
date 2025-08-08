use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::time::{interval, MissedTickBehavior};

#[derive(Clone)]
struct Config {
    base: String,
    mode: String,       // "insert" | "build"
    concurrency: usize, // worker tasks
    rps: u64,           // producer rate
    duration: Duration, // run duration
    seed: i32,          // mempool seed for build mode
    api_key: Option<String>,
}

fn getenv<T: std::str::FromStr>(k: &str, default: T) -> T {
    std::env::var(k)
        .ok()
        .and_then(|s| s.parse::<T>().ok())
        .unwrap_or(default)
}

fn getenv_str(k: &str, default: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() {
    let cfg = Config {
        base: getenv_str("FPN_LOAD_TARGET", "http://127.0.0.1:8080"),
        mode: getenv_str("FPN_LOAD_MODE", "insert"),
        concurrency: getenv("FPN_LOAD_CONCURRENCY", 32usize),
        rps: getenv("FPN_LOAD_RPS", 200u64),
        duration: Duration::from_secs(getenv("FPN_LOAD_DURATION_SECS", 30u64)),
        seed: getenv("FPN_LOAD_SEED", 500i32),
        api_key: std::env::var("FPN_API_KEY").ok(),
    };

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(64)
        .tcp_keepalive(Some(Duration::from_secs(30)))
        .build()
        .expect("client");

    if cfg.mode == "build" {
        // Reset + seed mempool
        let _ = client.delete(format!("{}/reset", cfg.base)).send().await;
        let _ = client
            .post(format!("{}/seed", cfg.base))
            .json(&serde_json::json!({"count": cfg.seed}))
            .send()
            .await;
    }

    println!(
        "loader starting: mode={}, target={}, concurrency={}, rps={}, duration_secs={}",
        cfg.mode,
        cfg.base,
        cfg.concurrency,
        cfg.rps,
        cfg.duration.as_secs()
    );

    run_load(client, cfg).await;
}

async fn run_load(client: reqwest::Client, cfg: Config) {
    let (tx, rx) = mpsc::channel::<()>(1024);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let sem = Arc::new(Semaphore::new(cfg.concurrency));

    // producer
    let mut tick = interval(Duration::from_nanos(1_000_000_000 / cfg.rps.max(1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);

    let start = Instant::now();
    let producer = tokio::spawn({
        let tx = tx.clone();
        async move {
            loop {
                if start.elapsed() > cfg.duration {
                    break;
                }
                tick.tick().await;
                if tx.send(()).await.is_err() {
                    break;
                }
            }
        }
    });

    let successes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    // latency collector
    let (lat_tx, mut lat_rx) = mpsc::unbounded_channel::<u128>();
    let collector = tokio::spawn(async move {
        let mut latencies: Vec<u128> = Vec::new();
        while let Some(v) = lat_rx.recv().await {
            latencies.push(v);
        }
        latencies
    });

    // workers
    let mut workers = Vec::with_capacity(cfg.concurrency);
    for _ in 0..cfg.concurrency {
        let rx = rx.clone();
        let client = client.clone();
        let cfg = cfg.clone();
        let sem = sem.clone();
        let successes = successes.clone();
        let failures = failures.clone();
        let lat_tx = lat_tx.clone();
        workers.push(tokio::spawn(async move {
            let rx = rx;
            loop {
                let msg_opt = {
                    let mut guard = rx.lock().await;
                    guard.recv().await
                };
                let Some(()) = msg_opt else {
                    break;
                };
                let _permit = sem.acquire().await.expect("semaphore");
                let start = Instant::now();
                let res = match cfg.mode.as_str() {
                    "insert" => do_insert(&client, &cfg).await,
                    "build" => do_build(&client, &cfg).await,
                    _ => Ok(()),
                };
                let dur = start.elapsed().as_nanos();
                if res.is_ok() {
                    successes.fetch_add(1, Ordering::Relaxed);
                } else {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                let _ = lat_tx.send(dur);
            }
        }));
    }

    drop(tx); // allow workers to end when producer is done
    let _ = producer.await;
    for w in workers {
        let _ = w.await;
    }

    // close latency channel and collect
    drop(lat_tx);
    let mut latencies = collector.await.unwrap_or_default();

    // summarize
    latencies.sort_unstable();
    let count = latencies.len();
    let p = |q: f64| -> Option<f64> {
        if count == 0 {
            return None;
        }
        let idx = ((count as f64 - 1.0) * q) as usize;
        Some(latencies[idx] as f64 / 1_000_000.0)
    };
    println!(
        "done: ok={}, err={}",
        successes.load(Ordering::Relaxed),
        failures.load(Ordering::Relaxed)
    );
    if let Some(p50) = p(0.50) {
        println!("p50_ms={:.3}", p50);
    }
    if let Some(p95) = p(0.95) {
        println!("p95_ms={:.3}", p95);
    }
    if let Some(p99) = p(0.99) {
        println!("p99_ms={:.3}", p99);
    }
}

async fn do_insert(client: &reqwest::Client, cfg: &Config) -> anyhow::Result<()> {
    // Generate randomness before any await, and drop RNG immediately
    let (profit, resources) = {
        let mut rng = rand::thread_rng();
        let n: usize = rng.gen_range(1..=4);
        let resources: Vec<String> = (0..n).map(|i| format!("pair:ASSET{}", i)).collect();
        let profit: f64 = rng.gen_range(1.0..5.0);
        (profit, resources)
    };
    let body = serde_json::json!({
        "profit": profit,
        "resources": resources,
        "client_id": "loader"
    });
    let mut req = client.post(format!("{}/intents", cfg.base)).json(&body);
    if let Some(k) = &cfg.api_key {
        req = req.header("x-api-key", k);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    Ok(())
}

async fn do_build(client: &reqwest::Client, cfg: &Config) -> anyhow::Result<()> {
    let mut req = client
        .get(format!("{}/build_block", cfg.base))
        .query(&[("algorithm", "greedy"), ("fee_rate", "0.1")]);
    if let Some(k) = &cfg.api_key {
        req = req.header("x-api-key", k);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("status {}", resp.status());
    }
    Ok(())
}
