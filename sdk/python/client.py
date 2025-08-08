from __future__ import annotations

from typing import Any, Dict, List, Literal, Optional

import httpx

Algorithm = Literal["fifo", "greedy"]


class FlashpointClient:
    """
    Minimal synchronous Python SDK for the mock private mempool API.
    """

    def __init__(
        self,
        base_url: str = "http://127.0.0.1:8000",
        *,
        timeout: float = 10.0,
        client: Optional[httpx.Client] = None,
        api_key: Optional[str] = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._headers: Dict[str, str] = {}
        if api_key:
            self._headers["X-API-Key"] = api_key
        if client is not None:
            self._client = client
            self._owns_client = False
        else:
            self._client = httpx.Client(base_url=self.base_url, timeout=timeout)
            self._owns_client = True

    def close(self) -> None:
        if getattr(self, "_owns_client", False):
            self._client.close()

    # ---- utility ----
    def health(self) -> Dict[str, Any]:
        r = self._client.get("/health")
        r.raise_for_status()
        return r.json()

    def reset(self) -> Dict[str, Any]:
        r = self._client.delete("/reset", headers=self._headers)
        r.raise_for_status()
        return r.json()

    # ---- intents ----
    def list_intents(self) -> List[Dict[str, Any]]:
        r = self._client.get("/intents")
        r.raise_for_status()
        return r.json()

    def submit_intent(self, *, profit: float, resources: List[str], client_id: Optional[str] = None) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"profit": float(profit), "resources": list(resources)}
        if client_id is not None:
            payload["client_id"] = client_id
        r = self._client.post("/intents", json=payload, headers=self._headers)
        r.raise_for_status()
        return r.json()

    def submit_intents_bulk(self, intents: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        payload = {"intents": intents}
        r = self._client.post("/intents/bulk", json=payload, headers=self._headers)
        r.raise_for_status()
        return r.json()

    # ---- block building ----
    def build_block(self, algorithm: Algorithm = "greedy", *, fee_rate: float = 0.1) -> Dict[str, Any]:
        r = self._client.get(
            "/build_block",
            params={"algorithm": algorithm, "fee_rate": fee_rate},
            headers=self._headers,
        )
        r.raise_for_status()
        return r.json()

    def compare(self) -> Dict[str, Any]:
        r = self._client.get("/compare")
        r.raise_for_status()
        return r.json()

    def metrics(self) -> Dict[str, Any]:
        r = self._client.get("/metrics")
        r.raise_for_status()
        return r.json()


__all__ = ["FlashpointClient", "Algorithm"]
