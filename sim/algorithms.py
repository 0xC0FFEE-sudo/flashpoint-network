from __future__ import annotations

from typing import Iterable, List, Tuple

from .models import Intent, conflicts


def fifo_select(intents: Iterable[Intent]) -> List[Intent]:
    """
    Select intents in FIFO order (by arrival_index), skipping ones that conflict
    with already selected intents.
    """
    selected: List[Intent] = []
    used_resources = set()
    ordered = sorted(intents, key=lambda x: (x.arrival_index is None, x.arrival_index))
    for it in ordered:
        if not set(it.resources) & used_resources:
            selected.append(it)
            used_resources.update(it.resources)
    return selected


def greedy_max_profit_select(intents: Iterable[Intent]) -> List[Intent]:
    """
    Greedy heuristic for Maximum-Weight Independent Set on conflict graph:
    sort by descending profit and include if it doesn't conflict with chosen set.
    """
    selected: List[Intent] = []
    used_resources = set()
    for it in sorted(intents, key=lambda x: x.profit, reverse=True):
        if not set(it.resources) & used_resources:
            selected.append(it)
            used_resources.update(it.resources)
    return selected


def total_profit(intents: Iterable[Intent]) -> float:
    return float(sum(max(0.0, it.profit) for it in intents))


def compare_algorithms(intents: Iterable[Intent]) -> Tuple[float, float]:
    """Return (fifo_profit, greedy_profit)."""
    fifo = fifo_select(list(intents))
    greedy = greedy_max_profit_select(list(intents))
    return total_profit(fifo), total_profit(greedy)
