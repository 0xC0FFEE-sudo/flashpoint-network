from __future__ import annotations

import importlib
import os

import pytest

from sim.models import Intent
from sim.algorithms import greedy_max_profit_select, fifo_select, total_profit
import sim.engine as engine


def _sample_intents():
    return [
        Intent(profit=10.0, resources=["A", "B"], client_id="x", arrival_index=0),
        Intent(profit=7.0, resources=["B", "C"], client_id="y", arrival_index=1),
        Intent(profit=5.0, resources=["D"], client_id="z", arrival_index=2),
        Intent(profit=12.0, resources=["E"], client_id="w", arrival_index=3),
        Intent(profit=1.0, resources=["A"], client_id="v", arrival_index=4),
    ]


def test_build_block_parity_python_vs_accel():
    intents = _sample_intents()

    # Baseline profits (pure Python)
    py_fifo_profit = total_profit(fifo_select(intents))
    py_greedy_profit = total_profit(greedy_max_profit_select(intents))

    # Try enabling Rust accelerator and reload engine module to pick it up.
    os.environ["FPN_USE_RUST"] = "1"
    importlib.reload(engine)

    res_fifo = engine.build_block(intents, algorithm="fifo")
    res_greedy = engine.build_block(intents, algorithm="greedy")

    assert pytest.approx(res_fifo.total_profit) == py_fifo_profit
    assert pytest.approx(res_greedy.total_profit) == py_greedy_profit

    # Cleanup: ensure not to affect other tests
    os.environ.pop("FPN_USE_RUST", None)
    importlib.reload(engine)
