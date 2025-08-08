from __future__ import annotations

from fastapi.testclient import TestClient

from mempool.app import app


def test_index_served():
    client = TestClient(app)
    r = client.get("/")
    assert r.status_code == 200
    assert b"Flashpoint Mock Mempool" in r.content
