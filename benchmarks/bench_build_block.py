from __future__ import annotations

import argparse
import os
import random
import statistics
import time
from typing import List

# Ensure project root is on sys.path when running from benchmarks/
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from sim.models import Intent
from sim.engine import build_block, engine_mode


def gen_intents(n: int, resources_pool: int, resources_per_intent: int = 1) -> List[Intent]:
    intents: List[Intent] = []
    for i in range(n):
        resources = [f"pair:{random.randint(0, resources_pool-1)}" for _ in range(resources_per_intent)]
        intents.append(
            Intent(
                profit=float(random.uniform(1.0, 100.0)),
                resources=resources,
                client_id=None,
                arrival_index=i,
            )
        )
    return intents


def main() -> None:
    parser = argparse.ArgumentParser(description="Flashpoint build_block microbenchmark")
    parser.add_argument("-n", "--num", type=int, default=10000, help="number of intents")
    parser.add_argument("-r", "--resources-pool", type=int, default=2000, help="distinct resources in pool")
    parser.add_argument("-k", "--resources-per-intent", type=int, default=1, help="resources per intent")
    parser.add_argument("-a", "--algorithm", type=str, default="greedy", choices=["fifo", "greedy"])
    parser.add_argument("-w", "--warmup", type=int, default=2)
    parser.add_argument("-t", "--trials", type=int, default=5)
    args = parser.parse_args()

    random.seed(42)
    intents = gen_intents(args.num, args.resources_pool, args.resources_per_intent)

    # Warmup
    for _ in range(args.warmup):
        _ = build_block(intents, algorithm=args.algorithm)

    # Measure
    durations = []
    total_selected = 0
    for _ in range(args.trials):
        t0 = time.perf_counter()
        res = build_block(intents, algorithm=args.algorithm)
        dt = time.perf_counter() - t0
        durations.append(dt)
        total_selected += len(res.selected)

    avg = statistics.mean(durations)
    p50 = statistics.median(durations)
    p95 = statistics.quantiles(durations, n=20)[18] if len(durations) >= 2 else avg

    mode = engine_mode()
    print(f"engine_mode={mode}")
    print(f"N={args.num} algo={args.algorithm} resources_pool={args.resources_pool} k={args.resources_per_intent}")
    print(f"avg={avg*1000:.2f} ms  p50={p50*1000:.2f} ms  p95={p95*1000:.2f} ms  selected_per_trial~={total_selected//args.trials}")


if __name__ == "__main__":
    main()
