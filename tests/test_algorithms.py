from __future__ import annotations

from sim.algorithms import (
    fifo_select,
    greedy_max_profit_select,
    total_profit,
)
from sim.generator import random_intents
from sim.models import conflicts


def test_greedy_profit_at_least_fifo():
    intents = random_intents(n=30, conflict_rate=0.4, seed=123)
    fifo = fifo_select(intents)
    greedy = greedy_max_profit_select(intents)
    assert total_profit(greedy) >= total_profit(fifo)


def test_no_conflicts_in_selected_sets():
    intents = random_intents(n=25, conflict_rate=0.5, seed=7)
    for selector in (fifo_select, greedy_max_profit_select):
        selected = selector(intents)
        # Ensure no pair conflicts
        for i in range(len(selected)):
            for j in range(i + 1, len(selected)):
                assert not conflicts(selected[i], selected[j])
