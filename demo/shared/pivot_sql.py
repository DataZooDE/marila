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
from typing import Mapping, Optional


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


def build_pivot_sql(
    rows: str,
    cols: Optional[str],
    measure: str,
    where: Optional[str] = None,
    *,
    table: str = "lake.nyc.yellow",
    row_limit: int = 200,
) -> str:
    """Assemble a single-statement SQL pivot.

    - `rows`: dimension name (must be a key of `DIMENSIONS`).
    - `cols`: dimension name or None. None ⇒ no pivot, just a GROUP BY rows.
    - `measure`: measure name (key of `MEASURES`).
    - `where`: optional free-text WHERE clause (must not include the word "WHERE";
      the builder injects it). Passed through unmodified — caller / DuckDB's
      parser is the validator.
    - `table`: fully-qualified table reference (default `lake.nyc.yellow`).
    - `row_limit`: hard LIMIT on the outer query so the TUI doesn't try to
      render a 200k-row pivot.

    For 1-dim (no `cols`) the result is:

        SELECT <rows-expr> AS <rows>, <measure-expr> AS <measure>
          FROM <table> [WHERE <where>]
         GROUP BY 1
         ORDER BY <measure> DESC
         LIMIT <row_limit>;

    For 2-dim (with `cols`) we use DuckDB's PIVOT syntax (1.5.x):

        PIVOT (
          SELECT <rows-expr> AS <rows>, <cols-expr> AS <cols>, <measure-expr> AS <measure>
            FROM <table> [WHERE <where>]
        )
        ON <cols> USING FIRST(<measure>)
        GROUP BY <rows>
        ORDER BY <rows>
        LIMIT <row_limit>;
    """
    if rows not in DIMENSIONS:
        raise ValueError(
            f"unknown row dimension {rows!r} — pick one of {sorted(DIMENSIONS)}"
        )
    if measure not in MEASURES:
        raise ValueError(
            f"unknown measure {measure!r} — pick one of {sorted(MEASURES)}"
        )
    if cols is not None and cols not in DIMENSIONS:
        raise ValueError(
            f"unknown column dimension {cols!r} — pick one of {sorted(DIMENSIONS)}"
        )

    rd = DIMENSIONS[rows]
    md = MEASURES[measure]
    where_clause = f"WHERE {where}" if where and where.strip() else ""

    if cols is None:
        return (
            f"SELECT {rd.sql_expr} AS {rd.name}, "
            f"{md.sql_expr} AS {md.name} "
            f"FROM {table} {where_clause} "
            f"GROUP BY 1 "
            f"ORDER BY {md.name} DESC "
            f"LIMIT {row_limit}"
        ).replace("  ", " ").strip()

    cd = DIMENSIONS[cols]
    return (
        f"PIVOT ("
        f"SELECT {rd.sql_expr} AS {rd.name}, "
        f"{cd.sql_expr} AS {cd.name}, "
        f"{md.sql_expr} AS {md.name} "
        f"FROM {table} {where_clause}"
        f") "
        f"ON {cd.name} "
        f"USING FIRST({md.name}) "
        f"GROUP BY {rd.name} "
        f"ORDER BY {rd.name} "
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
