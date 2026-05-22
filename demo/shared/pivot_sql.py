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


def _normalize_inputs(
    rows: list[str] | str,
    cols: list[str] | str | None,
    measure: str,
) -> tuple[list[Dimension], list[Dimension], Measure]:
    """Shared validation for both build paths. Returns the
    resolved Dimension / Measure objects."""
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
    return [DIMENSIONS[r] for r in rows_list], [DIMENSIONS[c] for c in cols_list], MEASURES[measure]


def grouping_col_names(rows: list[str] | str) -> list[str]:
    """`day_of_week → _g_day_of_week`. The renderer uses these as the
    sentinel columns that mark hierarchy levels."""
    return [f"_g_{r}" for r in coerce_dim_list(rows)]


def build_pivot_sql(
    rows: list[str] | str,
    cols: list[str] | str | None,
    measure: str,
    where: Optional[str] = None,
    *,
    table: str = "lake.nyc.yellow",
    row_limit: int = 200,
    with_rollup: bool = True,
) -> str:
    """Assemble a single-statement SQL pivot with hierarchical rollup
    subtotals + a grand-TOTAL row.

    Output column order (left → right) is always:

        <row-dim-1>, …, <row-dim-N>,
        _g_<row-dim-1>, …, _g_<row-dim-N>,    -- GROUPING(...) flags
        <measure or spread columns>

    The `_g_*` columns are 1 when that dim was rolled up (NULL value
    in the row) and 0 when it carries an actual value. The renderer
    uses them to identify which level each row belongs to. Sort is
    pre-arranged so each parent comes before its children
    (`g DESC, dim NULLS FIRST` per dim).

    `with_rollup=False` falls back to a plain GROUP BY without
    subtotals — used by the test harness when we want a clean
    "leaves only" check.
    """
    row_dims, col_dims, md = _normalize_inputs(rows, cols, measure)
    where_clause = f"WHERE {where}" if where and where.strip() else ""
    row_names = [d.name for d in row_dims]
    row_selects = ", ".join(f"{d.sql_expr} AS {d.name}" for d in row_dims)
    grouping_selects = ", ".join(
        f"GROUPING({d.name}) AS _g_{d.name}" for d in row_dims
    )
    grouping_zero_selects = ", ".join(f"0 AS _g_{d.name}" for d in row_dims)
    row_group_clause = (
        f"ROLLUP({', '.join(row_names)})" if with_rollup else ", ".join(row_names)
    )
    order_clause = " ORDER BY " + ", ".join(
        f"_g_{d.name} DESC, {d.name} NULLS FIRST" for d in row_dims
    )

    # ── No pivot: plain SELECT with GROUP BY ROLLUP ──
    if not col_dims:
        if with_rollup:
            return (
                f"SELECT {row_selects}, {grouping_selects}, {md.sql_expr} AS {md.name} "
                f"FROM {table} {where_clause} "
                f"GROUP BY {row_group_clause} "
                f"{order_clause} "
                f"LIMIT {row_limit}"
            ).replace("  ", " ").strip()
        return (
            f"SELECT {row_selects}, {grouping_zero_selects}, {md.sql_expr} AS {md.name} "
            f"FROM {table} {where_clause} "
            f"GROUP BY {', '.join(row_names)} "
            f"ORDER BY " + (
                f"{md.name} DESC" if len(row_dims) == 1 else ", ".join(row_names)
            ) + f" "
            f"LIMIT {row_limit}"
        ).replace("  ", " ").strip()

    # ── PIVOT with rollup subtotals ──
    # DuckDB's PIVOT doesn't accept ROLLUP in its outer GROUP BY, so
    # we pre-aggregate via GROUPING SETS in an inner subquery, then
    # PIVOT spreads the col dims and uses FIRST() (each rolled-up
    # (level, col-dim) combination has exactly one source row).
    col_selects = ", ".join(f"{d.sql_expr} AS {d.name}" for d in col_dims)
    col_names = ", ".join(d.name for d in col_dims)
    # Enumerate the row-rollup levels as explicit GROUPING SETS.
    # For rows=[r1, r2, r3] this produces:
    #   (r1, r2, r3, c…), (r1, r2, c…), (r1, c…), (c…)
    if with_rollup:
        grouping_sets = []
        for level in range(len(row_dims), -1, -1):
            kept = row_names[:level]
            grouping_sets.append("(" + ", ".join(kept + [c.name for c in col_dims]) + ")")
        grouping_clause = (
            "GROUPING SETS (" + ", ".join(grouping_sets) + ")"
        )
    else:
        grouping_clause = ", ".join(row_names + [c.name for c in col_dims])
    inner = (
        f"WITH src AS ("
        f"SELECT {row_selects}, {col_selects}, * "
        f"FROM {table} {where_clause}"
        f"), "
        f"agg AS ("
        f"SELECT {', '.join(row_names)}, {col_names}, {md.sql_expr} AS {md.name}, "
        f"{grouping_selects} "
        f"FROM src "
        f"GROUP BY {grouping_clause}"
        f")"
    )
    # The PIVOT's outer GROUP BY has to include the _g_* columns so
    # rollup rows aren't accidentally re-aggregated.
    outer_group = ", ".join(row_names + [f"_g_{d.name}" for d in row_dims])
    return (
        f"{inner} "
        f"PIVOT agg ON {col_names} USING FIRST({md.name}) "
        f"GROUP BY {outer_group} "
        f"{order_clause} "
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
