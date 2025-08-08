from __future__ import annotations

import random
from typing import Iterable, List, Optional

from .models import Intent


DEFAULT_RESOURCE_PREFIXES = [
    "pair:ETH/USDC",
    "pair:WBTC/ETH",
    "pair:SOL/USDC",
    "pair:ARB/ETH",
    "dex:uni-v3",
    "dex:curve",
    "dex:raydium",
    "bridge:wormhole",
    "bridge:axelar",
]


def make_resource_pool(n: int, conflict_rate: float) -> List[str]:
    # Heuristic: fewer resources -> more conflicts; more resources -> fewer conflicts
    base = max(6, int(max(1.0, n * (1.0 - min(max(conflict_rate, 0.0), 1.0))) * 1.5))
    # Ensure at least as many as default prefixes
    pool = list(DEFAULT_RESOURCE_PREFIXES)
    # Add synthetic resources to reach target size
    i = 0
    while len(pool) < base:
        pool.append(f"res:{i}")
        i += 1
    return pool


def random_intents(
    n: int = 20,
    conflict_rate: float = 0.3,
    seed: Optional[int] = None,
    resources_pool: Optional[List[str]] = None,
) -> List[Intent]:
    """
    Generate random intents with approximate conflict characteristics.

    - profit ~ Uniform[1, 100]
    - each intent uses 1-2 resources sampled from a pool sized by conflict_rate
    - arrival_index is set from 0..n-1
    """
    if seed is not None:
        random.seed(seed)
    pool = resources_pool or make_resource_pool(n, conflict_rate)

    intents: List[Intent] = []
    for i in range(n):
        profit = round(random.uniform(1.0, 100.0), 2)
        k = 1 if random.random() < 0.6 else 2
        resources = random.sample(pool, k=k)
        intents.append(
            Intent(
                profit=profit,
                resources=resources,
                client_id=f"c{i}",
                arrival_index=i,
            )
        )
    return intents
