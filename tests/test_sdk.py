from __future__ import annotations

from fastapi.testclient import TestClient

from mempool.app import app
from sdk.python.client import FlashpointClient


def test_sdk_end_to_end_with_testclient():
    tc = TestClient(app)
    sdk = FlashpointClient(base_url="http://testserver", client=tc)  # type: ignore[arg-type]

    # basic flow
    assert sdk.health()["status"] == "ok"
    sdk.reset()

    sdk.submit_intent(profit=12.0, resources=["pair:ETH/USDC"], client_id="x")
    sdk.submit_intent(profit=25.0, resources=["pair:ETH/USDC"], client_id="y")
    sdk.submit_intent(profit=5.0, resources=["dex:uni-v3"], client_id="z")

    block = sdk.build_block(algorithm="greedy")
    profits = [it["profit"] for it in block["selected"]]
    assert 25.0 in profits and 5.0 in profits and 12.0 not in profits

    sdk.close()
