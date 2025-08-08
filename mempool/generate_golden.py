from __future__ import annotations

import json
from pathlib import Path
from typing import List

from sim.engine import build_block
from sim.algorithms import fifo_select, total_profit
from sim.models import Intent


def intents_list(intents: List[Intent]) -> List[dict]:
    return [
        {
            "profit": float(it.profit),
            "resources": list(it.resources),
            "client_id": it.client_id,
            "arrival_index": it.arrival_index,
        }
        for it in intents
    ]


def write_json(path: Path, obj) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        json.dump(obj, f, indent=2, sort_keys=True)
        f.write("\n")


def scenario_simple(out_dir: Path) -> None:
    # Two non-conflicting intents
    intents = [
        Intent(profit=10.0, resources=["pair:ETH/USDC"], client_id=None, arrival_index=0),
        Intent(profit=20.0, resources=["dex:uniswap"], client_id=None, arrival_index=1),
    ]
    # Golden for GET /intents after two POSTs
    write_json(out_dir / "simple_intents.json", intents_list(intents))

    # Golden for GET /build_block?algorithm=greedy&fee_rate=0.1
    algo = "greedy"
    fee_rate = 0.1
    res = build_block(intents, algorithm=algo)
    fifo_p = total_profit(fifo_select(intents))
    surplus = max(0.0, res.total_profit - fifo_p)
    sequencer_fee = surplus * fee_rate
    gross = [max(0.0, it.profit) for it in res.selected]
    total_gross = sum(gross) or 1.0
    selected = []
    for it, gp in zip(res.selected, gross):
        share = sequencer_fee * (gp / total_gross) if total_gross > 0 else 0.0
        selected.append({
            "profit": float(it.profit),
            "resources": list(it.resources),
            "client_id": it.client_id,
            "arrival_index": it.arrival_index,
            "fee_share": float(share),
            "trader_profit": float(max(0.0, it.profit) - share),
        })
    golden = {
        "algorithm": algo,
        "fee_rate": fee_rate,
        "baseline_fifo_profit": float(fifo_p),
        "total_profit": float(res.total_profit),
        "surplus": float(surplus),
        "sequencer_fee": float(sequencer_fee),
        "selected": selected,
    }
    write_json(out_dir / "simple_build_block_greedy_0.1.json", golden)


def scenario_conflict(out_dir: Path) -> None:
    # Three intents; 0 and 2 conflict, greedy should take 2 and 1
    intents = [
        Intent(profit=10.0, resources=["pair:ETH/USDC"], client_id=None, arrival_index=0),
        Intent(profit=20.0, resources=["dex:uniswap"], client_id=None, arrival_index=1),
        Intent(profit=25.0, resources=["pair:ETH/USDC"], client_id=None, arrival_index=2),
    ]
    write_json(out_dir / "conflict_intents.json", intents_list(intents))

    algo = "greedy"
    fee_rate = 0.1
    res = build_block(intents, algorithm=algo)
    fifo_p = total_profit(fifo_select(intents))
    surplus = max(0.0, res.total_profit - fifo_p)
    sequencer_fee = surplus * fee_rate
    gross = [max(0.0, it.profit) for it in res.selected]
    total_gross = sum(gross) or 1.0
    selected = []
    for it, gp in zip(res.selected, gross):
        share = sequencer_fee * (gp / total_gross) if total_gross > 0 else 0.0
        selected.append({
            "profit": float(it.profit),
            "resources": list(it.resources),
            "client_id": it.client_id,
            "arrival_index": it.arrival_index,
            "fee_share": float(share),
            "trader_profit": float(max(0.0, it.profit) - share),
        })
    golden = {
        "algorithm": algo,
        "fee_rate": fee_rate,
        "baseline_fifo_profit": float(fifo_p),
        "total_profit": float(res.total_profit),
        "surplus": float(surplus),
        "sequencer_fee": float(sequencer_fee),
        "selected": selected,
    }
    write_json(out_dir / "conflict_build_block_greedy_0.1.json", golden)


if __name__ == "__main__":
    out = Path("rust/server/tests/golden")
    scenario_simple(out)
    scenario_conflict(out)
