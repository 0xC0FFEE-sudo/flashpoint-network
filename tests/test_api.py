from __future__ import annotations

from fastapi.testclient import TestClient

from mempool.app import app


def test_api_flow():
    client = TestClient(app)

    # health
    r = client.get("/health")
    assert r.status_code == 200
    assert r.json()["status"] == "ok"

    # reset
    r = client.delete("/reset")
    assert r.status_code == 200

    # submit a few intents
    intents = [
        {"profit": 10.0, "resources": ["pair:ETH/USDC"], "client_id": "a"},
        {"profit": 30.0, "resources": ["pair:ETH/USDC"], "client_id": "b"},  # conflicts
        {"profit": 20.0, "resources": ["dex:uni-v3"], "client_id": "c"},
    ]
    for it in intents:
        r = client.post("/intents", json=it)
        assert r.status_code == 200

    # list
    r = client.get("/intents")
    assert r.status_code == 200
    assert len(r.json()) == 3

    # build block greedy should pick higher profit from conflict and include dex
    r = client.get("/build_block", params={"algorithm": "greedy"})
    assert r.status_code == 200
    data = r.json()
    assert data["algorithm"] == "greedy"
    profits = [it["profit"] for it in data["selected"]]
    assert 30.0 in profits and 20.0 in profits and 10.0 not in profits


def test_seed_endpoint_populates_mempool():
    client = TestClient(app)

    # ensure clean state
    r = client.delete("/reset")
    assert r.status_code == 200

    # seed 5 intents
    r = client.post("/seed", json={"count": 5})
    assert r.status_code == 200
    created = r.json()
    assert isinstance(created, list) and len(created) == 5

    # mempool should have at least 5
    r = client.get("/intents")
    assert r.status_code == 200
    assert len(r.json()) >= 5

    # metrics should be available
    r = client.get("/metrics")
    assert r.status_code == 200
