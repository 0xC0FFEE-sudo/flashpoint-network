from __future__ import annotations

import argparse
from .generator import random_intents
from .algorithms import compare_algorithms


def cmd_compare(args: argparse.Namespace) -> int:
    intents = random_intents(n=args.n, conflict_rate=args.conflict_rate, seed=args.seed)
    fifo, greedy = compare_algorithms(intents)
    improvement = greedy - fifo
    pct = (improvement / fifo * 100.0) if fifo > 0 else 0.0
    print(f"Intents: {args.n}, conflict_rate: {args.conflict_rate}, seed: {args.seed}")
    print(f"FIFO profit   : {fifo:.2f}")
    print(f"Greedy profit : {greedy:.2f}")
    print(f"Improvement   : {improvement:.2f} ({pct:.1f}%)")
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="flash-sim", description="Flashpoint simulator CLI")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_cmp = sub.add_parser("compare", help="Compare FIFO vs Greedy profits")
    p_cmp.add_argument("--n", type=int, default=20)
    p_cmp.add_argument("--conflict-rate", type=float, default=0.3)
    p_cmp.add_argument("--seed", type=int, default=42)
    p_cmp.set_defaults(func=cmd_compare)

    args = parser.parse_args(argv)
    return int(args.func(args) or 0)


if __name__ == "__main__":
    raise SystemExit(main())
