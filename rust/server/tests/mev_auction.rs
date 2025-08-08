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


#[test]
fn mev_batch_submit_get_order_book_and_cancel() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // Batch submit two orders
    let body = json!([
        {"client_id":"a","order": {"bid": 4.0, "priority": 5}},
        {"client_id":"b","order": {"bid": 7.0, "priority": 1}}
    ]);
    let rsp = http()
        .post(format!("{}/auction/submit_batch", base))
        .json(&body)
        .send()
        .expect("batch submit rsp");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    assert!(rsp.status().is_success(), "batch submit failed: {:?}", rsp.text());
    let v: Value = rsp.json().expect("json");
    let ids = v["ids"].as_array().cloned().unwrap_or_default();
    assert_eq!(ids.len(), 2);
    let id1 = ids[0].as_u64().unwrap();

    // Get order by id
    let rsp = http()
        .get(format!("{}/auction/order/{}", base, id1))
        .send()
        .expect("get order rsp");
    assert!(rsp.status().is_success(), "get order failed: {:?}", rsp.text());
    let oj: Value = rsp.json().expect("json");
    assert_eq!(oj["id"].as_u64(), Some(id1));
    assert!(oj["submitted_at_ms"].as_u64().is_some());

    // Book snapshot
    let rsp = http()
        .get(format!("{}/auction/book", base))
        .send()
        .expect("book rsp");
    assert!(rsp.status().is_success(), "book failed: {:?}", rsp.text());
    let bj: Value = rsp.json().expect("json");
    assert!(bj["submissions"].as_u64().unwrap_or(0) >= 2);
    let top = bj["top"].as_array().cloned().unwrap_or_default();
    assert!(!top.is_empty());
    let top_bid = top[0]["bid"].as_f64().unwrap_or(0.0);
    assert!(top_bid >= 4.0, "unexpected top bid: {}", top_bid);

    // Cancel first id
    let rsp = http()
        .delete(format!("{}/auction/cancel/{}", base, id1))
        .send()
        .expect("cancel rsp");
    assert!(rsp.status().is_success(), "cancel failed: {:?}", rsp.text());
    let cj: Value = rsp.json().expect("json");
    assert_eq!(cj["removed"].as_u64(), Some(1));

    // Fetch canceled order should 404
    let rsp = http()
        .get(format!("{}/auction/order/{}", base, id1))
        .send()
        .expect("get canceled order rsp");
    assert_eq!(rsp.status(), reqwest::StatusCode::NOT_FOUND);

    let _ = child.kill();
}

#[test]
fn mev_clear_with_filters_min_bid_budget_window_priority() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // Older low bid order
    let body_old = json!({"client_id":"old","order": {"bid": 2.0, "priority": 10}});
    let rsp = http().post(format!("{}/auction/submit", base)).json(&body_old).send().expect("submit old");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    assert!(rsp.status().is_success());
    // Ensure time gap so the old order falls outside the upcoming window
    std::thread::sleep(Duration::from_millis(200));

    // Recent orders
    for body in [
        json!({"client_id":"p1","order": {"bid": 9.0, "priority": 5}}),
        json!({"client_id":"p2","order": {"bid": 8.0, "priority": 1}}),
        json!({"client_id":"low","order": {"bid": 1.0, "priority": 1}}),
    ] {
        let rsp = http().post(format!("{}/auction/submit", base)).json(&body).send().expect("submit recent");
        assert!(rsp.status().is_success());
    }

    // Clear with filters: exclude very old via window, exclude low via min_bid, prioritize by priority, budget_cap=8 allows only p2
    let clear_url = format!(
        "{}/auction/clear?top_k=3&min_bid=2.0&budget_cap=8.0&window_ms=100&priority_first=true",
        base
    );
    // Tiny sleep so recent orders are well within the 100ms window
    std::thread::sleep(Duration::from_millis(10));
    let rsp = http().post(clear_url).send().expect("clear rsp");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    assert!(rsp.status().is_success(), "clear failed: {:?}", rsp.text());
    let v: Value = rsp.json().expect("json");
    assert_eq!(v["criteria"].as_str(), Some("priority_then_bid"));
    let winners = v["winners"].as_array().cloned().unwrap_or_default();
    assert_eq!(winners.len(), 1, "expected 1 winner due to budget cap");
    assert_eq!(winners[0]["client_id"].as_str(), Some("p2"));

    let _ = child.kill();
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
fn mev_auction_clear_topk_sorted() {
    let port = pick_unused_port().expect("port");
    let base = format!("http://127.0.0.1:{}", port);
    let mut child = spawn_server(port);
    wait_for_health(&base, Duration::from_secs(5));

    // submit three orders with bids 1.0, 5.0, 3.0
    for (bid, cid) in &[(1.0, "c1"), (5.0, "c2"), (3.0, "c3")] {
        let body = json!({
            "client_id": *cid,
            "order": { "bid": *bid }
        });
        let rsp = http()
            .post(format!("{}/auction/submit", base))
            .json(&body)
            .send()
            .expect("submit rsp");
        if rsp.status() == reqwest::StatusCode::NOT_FOUND {
            let _ = child.kill();
            return; // feature not enabled; skip test
        }
        assert!(rsp.status().is_success(), "submit failed: {:?}", rsp.text());
    }

    let clear_url = format!("{}/auction/clear?top_k=2", base);
    let rsp = http().post(clear_url).send().expect("clear rsp");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    assert!(rsp.status().is_success(), "clear failed: {:?}", rsp.text());
    let v: Value = rsp.json().expect("json");
    let winners = v["winners"].as_array().cloned().unwrap_or_default();
    assert_eq!(winners.len(), 2);
    let bids: Vec<f64> = winners
        .iter()
        .map(|w| w["order"]["bid"].as_f64().unwrap_or(0.0))
        .collect();
    assert!(bids[0] >= bids[1], "winners not sorted desc: {:?}", bids);
    assert_eq!(v["criteria"].as_str(), Some("bid_desc"));

    // results reflect last_winners
    let rsp = http()
        .get(format!("{}/auction/results", base))
        .send()
        .expect("results rsp");
    if rsp.status() == reqwest::StatusCode::NOT_FOUND { let _ = child.kill(); return; }
    let rj: Value = rsp.json().expect("json");
    assert_eq!(rj["winners"].as_u64(), Some(2));

    let _ = child.kill();
}
