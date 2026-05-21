#!/usr/bin/env python3
"""Generate a deterministic 1,000-row synthetic sales CSV for the
analytical demo. Deterministic = `random.seed(...)` so the assertions
in the analytics queries are reproducible across runs."""

from __future__ import annotations

import csv
import pathlib
import random
import sys
from datetime import date, timedelta

OUT_PATH = pathlib.Path(__file__).parent / "sales_seed.csv"

REGIONS = ["us-east", "us-west", "eu-west", "ap-south"]
PRODUCTS = [
    ("widget-basic", 1999, "widgets"),
    ("widget-pro", 9999, "widgets"),
    ("gadget-mini", 1499, "gadgets"),
    ("gadget-pro", 12999, "gadgets"),
    ("toolkit", 4999, "tools"),
    ("toolkit-deluxe", 19999, "tools"),
]
N_ROWS = 1_000
START = date(2026, 1, 1)
DAYS = 120


def main() -> int:
    random.seed(42)
    rows: list[dict] = []
    for i in range(N_ROWS):
        product, base_price, category = random.choice(PRODUCTS)
        region = random.choice(REGIONS)
        days_offset = random.randint(0, DAYS - 1)
        # Variability: occasional ±20 % price perturbation
        price = int(base_price * random.choice([0.8, 0.9, 1.0, 1.0, 1.0, 1.1, 1.2]))
        quantity = random.randint(1, 5)
        rows.append(
            {
                "order_id": i + 1,
                "order_date": (START + timedelta(days=days_offset)).isoformat(),
                "customer_id": 1000 + random.randint(0, 199),
                "region": region,
                "product": product,
                "category": category,
                "quantity": quantity,
                "amount_cents": price * quantity,
            }
        )

    # Salt a few intentionally-bad rows so the analytical demo can show
    # DELETE removing them:
    #   - one row with a negative amount
    #   - one row with an absurd future date
    rows.append({
        "order_id": 99001,
        "order_date": "2026-04-30",
        "customer_id": 1099,
        "region": "us-east",
        "product": "widget-basic",
        "category": "widgets",
        "quantity": 1,
        "amount_cents": -1500,
    })
    rows.append({
        "order_id": 99002,
        "order_date": "2199-12-31",
        "customer_id": 1099,
        "region": "us-east",
        "product": "gadget-mini",
        "category": "gadgets",
        "quantity": 1,
        "amount_cents": 1499,
    })

    with OUT_PATH.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(rows[0].keys()))
        writer.writeheader()
        for r in rows:
            writer.writerow(r)
    print(f"Wrote {len(rows)} rows to {OUT_PATH.relative_to(OUT_PATH.parent.parent)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
