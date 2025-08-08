from __future__ import annotations

from math import isclose

from fastapi.testclient import TestClient

from mempool.app import app


def test_fee_and_surplus_math():
    client = TestClient(app)
    client.delete("/reset")

    # Submit conflicting intents to create surplus potential
    client.post("/intents", json={"profit": 10.0, "resources": ["pair:ETH/USDC"]})
    client.post("/intents", json={"profit": 40.0, "resources": ["pair:ETH/USDC"]})  # higher profit
    client.post("/intents", json={"profit": 5.0, "resources": ["dex:uni-v3"]})

    fee_rate = 0.2
    r = client.get("/build_block", params={"algorithm": "greedy", "fee_rate": fee_rate})
    assert r.status_code == 200
    data = r.json()

    # expect baseline <= total and surplus = total - baseline
    assert data["total_profit"] >= data["baseline_fifo_profit"]
    expected_surplus = data["total_profit"] - data["baseline_fifo_profit"]
    assert isclose(data["surplus"], expected_surplus, rel_tol=1e-9, abs_tol=1e-9)
    assert isclose(data["sequencer_fee"], fee_rate * data["surplus"], rel_tol=1e-9, abs_tol=1e-9)

    # fee shares sum to sequencer_fee
    fee_sum = sum(item["fee_share"] for item in data["selected"])
    assert isclose(fee_sum, data["sequencer_fee"], rel_tol=1e-9, abs_tol=1e-9)


def test_compare_endpoint():
    client = TestClient(app)
    client.delete("/reset")
    # Create random-ish set
    client.post("/intents", json={"profit": 3.0, "resources": ["r1"]})
    client.post("/intents", json={"profit": 2.0, "resources": ["r1"]})  # conflicts
    client.post("/intents", json={"profit": 4.0, "resources": ["r2"]})

    r = client.get("/compare")
    assert r.status_code == 200
    cmp = r.json()
    assert cmp["greedy_profit"] >= cmp["fifo_profit"]
