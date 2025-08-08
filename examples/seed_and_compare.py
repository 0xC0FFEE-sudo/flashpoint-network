from __future__ import annotations

import random
import os
import sys
THIS_DIR = os.path.dirname(__file__)
ROOT = os.path.abspath(os.path.join(THIS_DIR, ".."))
if ROOT not in sys.path:
    sys.path.insert(0, ROOT)
from sdk.python.client import FlashpointClient


def main() -> None:
    sdk = FlashpointClient()
    try:
        print("Health:", sdk.health())
        sdk.reset()
        random.seed(1)
        # Seed some randomish intents
        resources_pool = [
            "pair:ETH/USDC",
            "pair:WBTC/ETH",
            "dex:uni-v3",
            "dex:curve",
            "bridge:wormhole",
        ]
        for i in range(12):
            profit = round(random.uniform(2, 60), 2)
            r1 = random.choice(resources_pool)
            if random.random() < 0.4:
                # sometimes add a second resource to introduce conflicts
                r2 = random.choice(resources_pool)
                resources = list({r1, r2})
            else:
                resources = [r1]
            sdk.submit_intent(profit=profit, resources=resources, client_id=f"c{i}")

        cmp = sdk.compare()
        print("Baseline FIFO:", cmp["fifo_profit"]) 
        print("Greedy Profit :", cmp["greedy_profit"]) 
        block = sdk.build_block(algorithm="greedy", fee_rate=0.12)
        print("\nBlock Summary")
        print("Algorithm      :", block["algorithm"])
        print("Fee Rate       :", block["fee_rate"])
        print("FIFO Baseline  :", block["baseline_fifo_profit"])
        print("Total Profit   :", block["total_profit"])
        print("Surplus        :", block["surplus"])
        print("Sequencer Fee  :", block["sequencer_fee"])
        print("Selected Intents (profit -> trader_profit, fee_share):")
        for it in block["selected"]:
            print(f"  {it['profit']:6.2f} -> {it['trader_profit']:6.2f}, fee {it['fee_share']:6.2f}  resources={it['resources']}")
    finally:
        sdk.close()


if __name__ == "__main__":
    main()
