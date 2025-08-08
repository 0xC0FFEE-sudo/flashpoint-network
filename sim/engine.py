from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Callable, Iterable, List, Literal, Tuple

from .algorithms import (
    fifo_select,
    greedy_max_profit_select,
    total_profit,
)
from .models import Intent


AlgorithmName = Literal["fifo", "greedy"]


ALGOS: dict[AlgorithmName, Callable[[Iterable[Intent]], List[Intent]]] = {
    "fifo": fifo_select,
    "greedy": greedy_max_profit_select,
}

# Optional Rust-backed accelerator via PyO3 (enabled with FPN_USE_RUST=1)
_USE_RUST = os.environ.get("FPN_USE_RUST") == "1"
try:  # pragma: no cover - optional dependency
    if _USE_RUST:
        import fpn_engine  # type: ignore
    else:
        fpn_engine = None  # type: ignore
except Exception:  # pragma: no cover
    fpn_engine = None  # type: ignore


@dataclass
class BlockResult:
    selected: List[Intent]
    total_profit: float
    algorithm: AlgorithmName


def engine_mode() -> str:
    """Return 'rust' if the PyO3 accelerator is active, else 'python'."""
    return "rust" if 'fpn_engine' in globals() and fpn_engine is not None else "python"


def build_block(intents: Iterable[Intent], algorithm: AlgorithmName = "greedy") -> BlockResult:
    # Fast path: Rust engine if enabled and available
    if fpn_engine is not None:
        try:
            py_list = [
                {
                    "profit": float(it.profit),
                    "resources": list(it.resources),
                    "client_id": it.client_id,
                    "arrival_index": it.arrival_index,
                }
                for it in intents
            ]
            selected_dicts, profit, algo = fpn_engine.build_block(py_list, algorithm)
            selected = [Intent(**d) for d in selected_dicts]
            return BlockResult(selected=selected, total_profit=float(profit), algorithm=algo)
        except Exception:
            # Fallback to Python if anything goes wrong
            pass

    # Default Python implementation
    selector = ALGOS.get(algorithm, greedy_max_profit_select)
    selected = selector(list(intents))
    return BlockResult(selected=selected, total_profit=total_profit(selected), algorithm=algorithm)
