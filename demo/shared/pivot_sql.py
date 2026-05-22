"""SQL pivot-query builder.

One shared assembler used by:
  - `tables/agent.py`'s `pivot` tool — so the LLM only picks
    dimensions + measure, doesn't have to write CASE/PIVOT syntax.
  - `tables/chat.py`'s controls pane — F5 "run" assembles the same
    SQL through the same code path, guaranteeing the agentic + manual
    flows produce identical output.

Dimensions and measures are passed by *display name* — the builder
maps them to SQL expressions via the `DIMENSIONS` / `MEASURES` tables
below. That keeps the model's tool calls human-readable
(`pivot(rows="hour_of_day", cols="payment_type", measure="trip_count")`)
and decouples them from the underlying Iceberg column names.

Tables-side WHERE strings are passed through *unmodified* — DuckDB's
parser is the validator. This is fine because the demo is read-only;
in a real product we'd want a sqlglot pass to prevent injection from
the model into the SQL planner.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Mapping, Optional


@dataclass(frozen=True)
class Dimension:
    """One named row/column dimension users can pivot by."""

    name: str                # human-readable label ("hour_of_day")
    sql_expr: str            # SQL expression evaluated per row ("hour(tpep_pickup_datetime)")
    description: str         # one-liner for the LLM


@dataclass(frozen=True)
class Measure:
    """One named aggregate."""

    name: str                # ("trip_count", "avg_fare")
    sql_expr: str            # ("COUNT(*)", "AVG(fare_amount)")
    description: str


# ---------------------------------------------------------------------------
# Canonical NYC Yellow Taxi dimensions + measures
# ---------------------------------------------------------------------------

DIMENSIONS: Mapping[str, Dimension] = {
    d.name: d
    for d in [
        Dimension(
            "vendor_id",
            "vendorid",
            "Taxi company / TPEP vendor id (1=Creative Mobile, 2=VeriFone).",
        ),
        Dimension(
            "hour_of_day",
            "hour(tpep_pickup_datetime)",
            "Pickup hour 0-23.",
        ),
        Dimension(
            "day_of_week",
            "dayname(tpep_pickup_datetime)",
            "Pickup weekday name (Monday..Sunday).",
        ),
        Dimension(
            "pickup_date",
            "date_trunc('day', tpep_pickup_datetime)",
            "Pickup calendar day.",
        ),
        Dimension(
            "pickup_month",
            "date_trunc('month', tpep_pickup_datetime)",
            "Pickup calendar month.",
        ),
        Dimension(
            "pickup_location_id",
            "pulocationid",
            "TLC pickup zone id (1..265). Join with `taxi_zone_lookup` for human-readable.",
        ),
        Dimension(
            "dropoff_location_id",
            "dolocationid",
            "TLC dropoff zone id.",
        ),
        Dimension(
            "payment_type",
            (
                "CASE payment_type WHEN 1 THEN 'Credit card' "
                "WHEN 2 THEN 'Cash' WHEN 3 THEN 'No charge' "
                "WHEN 4 THEN 'Dispute' WHEN 5 THEN 'Unknown' "
                "WHEN 6 THEN 'Voided' ELSE 'Other' END"
            ),
            "How the trip was paid (Credit card / Cash / ...).",
        ),
        Dimension(
            "passenger_bucket",
            (
                "CASE WHEN passenger_count <= 1 THEN '1 pax' "
                "WHEN passenger_count <= 2 THEN '2 pax' "
                "WHEN passenger_count <= 4 THEN '3-4 pax' "
                "ELSE '5+ pax' END"
            ),
            "Bucketed passenger count: 1, 2, 3-4, 5+.",
        ),
        Dimension(
            "trip_distance_bucket",
            (
                "CASE WHEN trip_distance < 1 THEN '<1 mi' "
                "WHEN trip_distance < 3 THEN '1-3 mi' "
                "WHEN trip_distance < 10 THEN '3-10 mi' "
                "WHEN trip_distance < 30 THEN '10-30 mi' "
                "ELSE '30+ mi' END"
            ),
            "Bucketed trip distance.",
        ),
    ]
}


MEASURES: Mapping[str, Measure] = {
    m.name: m
    for m in [
        Measure("trip_count", "COUNT(*)", "Number of trips."),
        Measure(
            "total_revenue", "SUM(total_amount)", "Sum of `total_amount` in USD."
        ),
        Measure(
            "avg_fare", "AVG(fare_amount)", "Average `fare_amount` per trip in USD."
        ),
        Measure(
            "avg_total", "AVG(total_amount)", "Average `total_amount` per trip in USD."
        ),
        Measure(
            "avg_distance", "AVG(trip_distance)", "Average trip distance in miles."
        ),
        Measure(
            "avg_tip", "AVG(tip_amount)", "Average tip in USD."
        ),
        Measure(
            "avg_passengers",
            "AVG(passenger_count)",
            "Average passenger count per trip.",
        ),
    ]
}


# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------


def coerce_dim_list(x: Any) -> list[str]:
    """Accept whatever the caller passed and produce a flat list of
    dimension names. Tolerant of the LLM passing a single string
    instead of an array — common with smaller tool-use models."""
    if x is None:
        return []
    if isinstance(x, str):
        return [s.strip() for s in x.split(",") if s.strip()]
    return [str(s).strip() for s in x if str(s).strip()]


def build_pivot_sql(
    rows: list[str] | str,
    cols: list[str] | str | None,
    measure: str,
    where: Optional[str] = None,
    *,
    table: str = "lake.nyc.yellow",
    row_limit: int = 200,
) -> str:
    """Assemble a single-statement SQL pivot with arbitrary numbers of
    row + column dimensions.

    - `rows`: list of dimension names (or comma-separated string). At least one.
    - `cols`: list of dimension names (or comma-separated string), or None /
      empty list for "no pivot, just GROUP BY rows".
    - `measure`: measure name (key of `MEASURES`).
    - `where`: optional free-text WHERE body. Passed through verbatim.
    - `row_limit`: hard LIMIT on the outer query.

    Shapes:

      cols == [] / None  →  plain GROUP BY:
        SELECT r1-expr AS r1, …, rN-expr AS rN, measure-expr AS measure
          FROM table [WHERE …]
         GROUP BY 1, …, N
         ORDER BY <last column>
         LIMIT N;

      cols == [c1, …, cM]  →  DuckDB PIVOT with multi-column ON:
        PIVOT (
          SELECT r1, …, rN, c1, …, cM, measure
            FROM table [WHERE …]
           GROUP BY 1, …, N+M
        ) ON c1, …, cM USING FIRST(measure)
        GROUP BY r1, …, rN
        ORDER BY r1, …, rN
        LIMIT N;

    Multi-column `ON` is DuckDB's native cross-product spread: the
    output has one column per distinct (c1-val, c2-val, …) tuple.
    """
    rows_list = coerce_dim_list(rows)
    cols_list = coerce_dim_list(cols)
    if not rows_list:
        raise ValueError("at least one row dimension required")
    unknown_rows = [r for r in rows_list if r not in DIMENSIONS]
    if unknown_rows:
        raise ValueError(
            f"unknown row dimension(s) {unknown_rows} — pick from {sorted(DIMENSIONS)}"
        )
    unknown_cols = [c for c in cols_list if c not in DIMENSIONS]
    if unknown_cols:
        raise ValueError(
            f"unknown col dimension(s) {unknown_cols} — pick from {sorted(DIMENSIONS)}"
        )
    overlap = set(rows_list) & set(cols_list)
    if overlap:
        raise ValueError(
            f"dimension(s) {sorted(overlap)} appear in both rows and cols"
        )
    if measure not in MEASURES:
        raise ValueError(
            f"unknown measure {measure!r} — pick one of {sorted(MEASURES)}"
        )
    # Dedupe within each list while preserving order.
    seen: set[str] = set()
    rows_list = [r for r in rows_list if not (r in seen or seen.add(r))]
    seen = set()
    cols_list = [c for c in cols_list if not (c in seen or seen.add(c))]

    row_dims = [DIMENSIONS[r] for r in rows_list]
    col_dims = [DIMENSIONS[c] for c in cols_list]
    md = MEASURES[measure]
    where_clause = f"WHERE {where}" if where and where.strip() else ""
    row_selects = ", ".join(f"{d.sql_expr} AS {d.name}" for d in row_dims)
    row_names = ", ".join(d.name for d in row_dims)

    # ── No pivot: plain GROUP BY ──
    if not col_dims:
        positions = ", ".join(str(i + 1) for i in range(len(row_dims)))
        # 1-dim: sort by the measure descending (best leaderboard UX).
        # N-dim: sort by row dimensions in declared order (predictable).
        order_clause = (
            f"ORDER BY {md.name} DESC" if len(row_dims) == 1 else f"ORDER BY {row_names}"
        )
        return (
            f"SELECT {row_selects}, {md.sql_expr} AS {md.name} "
            f"FROM {table} {where_clause} "
            f"GROUP BY {positions} "
            f"{order_clause} "
            f"LIMIT {row_limit}"
        ).replace("  ", " ").strip()

    # ── PIVOT: multi-column ON spread ──
    col_selects = ", ".join(f"{d.sql_expr} AS {d.name}" for d in col_dims)
    col_names = ", ".join(d.name for d in col_dims)
    inner_positions = ", ".join(
        str(i + 1) for i in range(len(row_dims) + len(col_dims))
    )
    return (
        f"PIVOT ("
        f"SELECT {row_selects}, {col_selects}, {md.sql_expr} AS {md.name} "
        f"FROM {table} {where_clause} "
        f"GROUP BY {inner_positions}"
        f") "
        f"ON {col_names} "
        f"USING FIRST({md.name}) "
        f"GROUP BY {row_names} "
        f"ORDER BY {row_names} "
        f"LIMIT {row_limit}"
    ).replace("  ", " ").strip()


def list_dimensions() -> list[dict[str, str]]:
    """Return all dimensions as plain dicts — used by the LLM's schema
    helper and the controls pane's Select options."""
    return [
        {"name": d.name, "sql_expr": d.sql_expr, "description": d.description}
        for d in DIMENSIONS.values()
    ]


def list_measures() -> list[dict[str, str]]:
    return [
        {"name": m.name, "sql_expr": m.sql_expr, "description": m.description}
        for m in MEASURES.values()
    ]
