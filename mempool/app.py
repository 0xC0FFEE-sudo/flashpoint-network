from __future__ import annotations

import os
import threading
import asyncio
import random
from typing import List, Literal, Optional

from fastapi import FastAPI, HTTPException, Query, Depends, Header
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles
from fastapi import WebSocket, WebSocketDisconnect
from starlette.middleware.cors import CORSMiddleware
from pydantic import BaseModel

from sim.engine import build_block, AlgorithmName
from sim.algorithms import fifo_select, total_profit
from sim.models import Intent

# Observability: Prometheus
try:
    from prometheus_client import Counter, Gauge, Histogram, generate_latest, CONTENT_TYPE_LATEST
    _PROM_AVAILABLE = True
except Exception:  # pragma: no cover
    _PROM_AVAILABLE = False

from fastapi import Response

# Tracing: OpenTelemetry (optional)
try:  # pragma: no cover - best-effort instrumentation
    from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
    _OTEL_AVAILABLE = True
except Exception:
    _OTEL_AVAILABLE = False

# Default resource pool used by seeding utility
RESOURCE_POOL = [
    "pair:ETH/USDC",
    "pair:WBTC/ETH",
    "pair:SOL/USDC",
    "dex:uniswap",
    "dex:curve",
    "bridge:wormhole",
    "bridge:layerzero",
]


class SubmitIntent(BaseModel):
    profit: float
    resources: List[str]
    client_id: Optional[str] = None


class IntentPayout(BaseModel):
    # Echo of selected intent plus payout fields
    profit: float
    resources: List[str]
    client_id: Optional[str] = None
    arrival_index: Optional[int] = None
    fee_share: float
    trader_profit: float


class BlockResponse(BaseModel):
    algorithm: AlgorithmName
    fee_rate: float
    baseline_fifo_profit: float
    total_profit: float
    surplus: float
    sequencer_fee: float
    selected: List[IntentPayout]


app = FastAPI(title="Flashpoint Mock Private Mempool", version="0.2.0")

# Static dashboard
app.mount("/static", StaticFiles(directory="mempool/static"), name="static")


@app.get("/", include_in_schema=False)
def index_root():
    return FileResponse("mempool/static/index.html")

# CORS (allow all for prototype)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)

# Optional API key auth (only if env var is set)
API_KEY = os.environ.get("FPN_API_KEY")


def require_api_key(x_api_key: Optional[str] = Header(default=None, alias="X-API-Key")):
    if API_KEY and x_api_key != API_KEY:
        raise HTTPException(status_code=401, detail="invalid API key")
    return True


# WebSocket manager for real-time notifications
class _WSManager:
    def __init__(self) -> None:
        self._clients: set[WebSocket] = set()
        self._lock = threading.Lock()

    async def connect(self, ws: WebSocket) -> None:
        await ws.accept()
        with self._lock:
            self._clients.add(ws)
        # Update Prometheus gauge for WS clients if available
        if _PROM_AVAILABLE:
            g = globals().get("WS_CLIENTS")
            if g is not None:
                try:
                    g.set(len(self._clients))
                except Exception:
                    pass

    def disconnect(self, ws: WebSocket) -> None:
        with self._lock:
            if ws in self._clients:
                self._clients.remove(ws)
        # Update Prometheus gauge for WS clients if available
        if _PROM_AVAILABLE:
            g = globals().get("WS_CLIENTS")
            if g is not None:
                try:
                    g.set(len(self._clients))
                except Exception:
                    pass

    async def broadcast(self, message: dict) -> None:
        # Copy to avoid race while iterating
        with self._lock:
            clients = list(self._clients)
        to_drop: List[WebSocket] = []
        for ws in clients:
            try:
                await ws.send_json(message)
            except Exception:
                to_drop.append(ws)
        for ws in to_drop:
            self.disconnect(ws)


WS = _WSManager()


@app.websocket("/ws")
async def websocket_endpoint(ws: WebSocket):
    await WS.connect(ws)
    try:
        while True:
            # Keep alive: ignore any incoming messages
            await ws.receive_text()
    except WebSocketDisconnect:
        WS.disconnect(ws)


class _State:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._intents: List[Intent] = []
        self._next_index: int = 0

    def reset(self) -> None:
        with self._lock:
            self._intents.clear()
            self._next_index = 0

    def add_intent(self, req: SubmitIntent) -> Intent:
        with self._lock:
            it = Intent(
                profit=float(req.profit),
                resources=list(req.resources),
                client_id=req.client_id,
                arrival_index=self._next_index,
            )
            self._intents.append(it)
            self._next_index += 1
            return it

    def add_intents_bulk(self, reqs: List[SubmitIntent]) -> List[Intent]:
        created: List[Intent] = []
        with self._lock:
            for req in reqs:
                it = Intent(
                    profit=float(req.profit),
                    resources=list(req.resources),
                    client_id=req.client_id,
                    arrival_index=self._next_index,
                )
                self._intents.append(it)
                created.append(it)
                self._next_index += 1
        return created

    def list_intents(self) -> List[Intent]:
        with self._lock:
            return list(self._intents)

    def build_block(self, algorithm: AlgorithmName, fee_rate: float) -> BlockResponse:
        with self._lock:
            res = build_block(self._intents, algorithm=algorithm)
            fifo = fifo_select(self._intents)
            fifo_profit = total_profit(fifo)

        surplus = max(0.0, res.total_profit - fifo_profit)
        sequencer_fee = surplus * fee_rate

        # Distribute fee across selected intents proportionally to gross profit
        gross_profits = [max(0.0, it.profit) for it in res.selected]
        total_gross = sum(gross_profits) or 1.0
        payouts: List[IntentPayout] = []
        for it, gp in zip(res.selected, gross_profits):
            share = sequencer_fee * (gp / total_gross)
            payouts.append(
                IntentPayout(
                    profit=it.profit,
                    resources=it.resources,
                    client_id=it.client_id,
                    arrival_index=it.arrival_index,
                    fee_share=share,
                    trader_profit=max(0.0, it.profit) - share,
                )
            )

        return BlockResponse(
            algorithm=res.algorithm,
            fee_rate=fee_rate,
            baseline_fifo_profit=fifo_profit,
            total_profit=res.total_profit,
            surplus=surplus,
            sequencer_fee=sequencer_fee,
            selected=payouts,
        )


STATE = _State()

# Initialize metrics (if available)
if _PROM_AVAILABLE:
    INTENTS_SUBMITTED = Counter("fpn_intents_submitted_total", "Number of intents submitted")
    BLOCKS_BUILT = Counter("fpn_blocks_built_total", "Number of blocks built")
    MEMPOOL_SIZE = Gauge("fpn_mempool_intents", "Number of intents currently in mempool")
    BLOCK_PROFIT = Histogram("fpn_block_total_profit", "Total profit of built blocks")
    BLOCK_SURPLUS = Histogram("fpn_block_surplus", "Surplus over FIFO baseline")
    WS_CLIENTS = Gauge("fpn_ws_clients", "Number of connected websocket clients")


@app.get("/health")
def health():
    return {"status": "ok"}


@app.delete("/reset")
async def reset(_: bool = Depends(require_api_key)):
    STATE.reset()
    if _PROM_AVAILABLE:
        MEMPOOL_SIZE.set(0)
    await WS.broadcast({"type": "reset"})
    return {"reset": True}


@app.get("/intents", response_model=List[Intent])
def list_intents():
    intents = STATE.list_intents()
    if _PROM_AVAILABLE:
        MEMPOOL_SIZE.set(len(intents))
    return intents


@app.post("/intents", response_model=Intent)
async def submit_intent(req: SubmitIntent, _: bool = Depends(require_api_key)):
    if req.profit <= 0:
        raise HTTPException(status_code=400, detail="profit must be positive")
    if not req.resources:
        raise HTTPException(status_code=400, detail="resources must be non-empty")
    it = STATE.add_intent(req)
    if _PROM_AVAILABLE:
        INTENTS_SUBMITTED.inc()
        MEMPOOL_SIZE.set(len(STATE.list_intents()))
    await WS.broadcast({"type": "intent_submitted", "intent": it.model_dump()})
    return it


class BulkSubmitRequest(BaseModel):
    intents: List[SubmitIntent]


@app.post("/intents/bulk", response_model=List[Intent])
async def submit_intents_bulk(req: BulkSubmitRequest, _: bool = Depends(require_api_key)):
    if not req.intents:
        raise HTTPException(status_code=400, detail="intents must be non-empty")
    for it in req.intents:
        if it.profit <= 0:
            raise HTTPException(status_code=400, detail="all profits must be positive")
        if not it.resources:
            raise HTTPException(status_code=400, detail="all resources must be non-empty")
    created = STATE.add_intents_bulk(req.intents)
    if _PROM_AVAILABLE:
        INTENTS_SUBMITTED.inc(len(created))
        MEMPOOL_SIZE.set(len(STATE.list_intents()))
    await WS.broadcast({"type": "intents_bulk_submitted", "count": len(created)})
    return created


# Seed endpoint to quickly populate mempool with random intents
class SeedRequest(BaseModel):
    count: int = 50
    min_profit: float = 5.0
    max_profit: float = 120.0
    resource_pool: Optional[List[str]] = None


@app.post("/seed", response_model=List[Intent])
async def seed_mempool(req: SeedRequest, _: bool = Depends(require_api_key)):
    if req.count <= 0:
        raise HTTPException(status_code=400, detail="count must be positive")
    if req.min_profit <= 0 or req.max_profit <= 0 or req.max_profit < req.min_profit:
        raise HTTPException(status_code=400, detail="invalid profit range")
    pool = req.resource_pool or RESOURCE_POOL
    if not pool:
        raise HTTPException(status_code=400, detail="resource pool must be non-empty")
    # Generate intents
    intents: List[SubmitIntent] = []
    for i in range(req.count):
        profit = round(random.uniform(req.min_profit, req.max_profit), 2)
        k = random.randint(1, 2)
        intents.append(
            SubmitIntent(
                profit=profit,
                resources=random.sample(pool, k=k),
                client_id=f"seed-{i % 5}",
            )
        )
    created = STATE.add_intents_bulk(intents)
    if _PROM_AVAILABLE:
        INTENTS_SUBMITTED.inc(len(created))
        MEMPOOL_SIZE.set(len(STATE.list_intents()))
    await WS.broadcast({"type": "intents_bulk_submitted", "count": len(created)})
    return created


@app.get("/build_block", response_model=BlockResponse)
async def api_build_block(
    algorithm: AlgorithmName = Query("greedy", pattern="^(fifo|greedy)$"),
    fee_rate: float = Query(0.1, ge=0.0, le=1.0),
    _: bool = Depends(require_api_key),
):
    res = STATE.build_block(algorithm, fee_rate)
    if _PROM_AVAILABLE:
        BLOCKS_BUILT.inc()
        BLOCK_PROFIT.observe(res.total_profit)
        BLOCK_SURPLUS.observe(res.surplus)
        MEMPOOL_SIZE.set(len(STATE.list_intents()))
    await WS.broadcast({
        "type": "block_built",
        "algorithm": res.algorithm,
        "fee_rate": res.fee_rate,
        "baseline_fifo_profit": res.baseline_fifo_profit,
        "total_profit": res.total_profit,
        "surplus": res.surplus,
        "sequencer_fee": res.sequencer_fee,
        "selected_count": len(res.selected),
    })
    return res


class CompareResponse(BaseModel):
    fifo_profit: float
    greedy_profit: float


@app.get("/compare", response_model=CompareResponse)
def compare():
    intents = STATE.list_intents()
    fifo = total_profit(fifo_select(intents))
    greedy = total_profit(build_block(intents, algorithm="greedy").selected)
    return CompareResponse(fifo_profit=fifo, greedy_profit=greedy)


class MetricsResponse(BaseModel):
    intents_count: int
    distinct_resources: int
    fifo_profit: float
    greedy_profit: float
    surplus: float


@app.get("/metrics", response_model=MetricsResponse)
def metrics():
    intents = STATE.list_intents()
    fifo_profit = total_profit(fifo_select(intents))
    greedy_selected = build_block(intents, algorithm="greedy").selected
    greedy_profit = total_profit(greedy_selected)
    resources = set([r for it in intents for r in it.resources])
    return MetricsResponse(
        intents_count=len(intents),
        distinct_resources=len(resources),
        fifo_profit=fifo_profit,
        greedy_profit=greedy_profit,
        surplus=max(0.0, greedy_profit - fifo_profit),
    )

# Expose Prometheus scrape endpoint (separate from JSON metrics)
if _PROM_AVAILABLE:
    @app.get("/metrics/prometheus")
    def prom_metrics():  # pragma: no cover - basic passthrough
        return Response(generate_latest(), media_type=CONTENT_TYPE_LATEST)

# Enable OpenTelemetry auto-instrumentation if available
if _OTEL_AVAILABLE:  # pragma: no cover
    try:
        FastAPIInstrumentor.instrument_app(app)
    except Exception:
        pass
