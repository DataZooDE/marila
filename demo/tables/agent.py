"""Agent loop for the NYC Yellow Taxi tables-side TUI.

Mirrors `demo/vector/agent.py` shape but targets SQL on an Iceberg
table reached via marila's `/iceberg/v1/*` proxy:

  user → TUI → run_turn() → Ollama gemma4 ← tools: schema / run_sql / pivot
                                              │
                                              ▼
                                          DuckDB
                                              │
                                              ▼
                                 marila /iceberg/* proxy
                                              │
                                              ▼
                                       Lakekeeper + RustFS

All three tools share a single in-process DuckDB connection that has
already done `INSTALL iceberg; LOAD iceberg; CREATE SECRET … ; ATTACH
… AS lake`. Queries run as `SELECT … FROM lake.nyc.yellow`.
"""

from __future__ import annotations

import json
import os
import textwrap
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

import duckdb
import ollama

from shared.pivot_sql import (
    DIMENSIONS,  # noqa: F401 — re-exported for callers
    MEASURES,    # noqa: F401 — re-exported for callers
    build_pivot_sql,
    coerce_dim_list,
    list_dimensions,
    list_measures,
)


# ---------------------------------------------------------------------------
# Config (env-overridable, all defaults match `tables/load.sh`)
# ---------------------------------------------------------------------------

MARILA_ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
MARILA_REGION = os.environ.get("MARILA_REGION", "eu-west-1")
MARILA_ACCESS_KEY = os.environ.get("AWS_ACCESS_KEY_ID", "marila")
MARILA_SECRET = os.environ.get("AWS_SECRET_ACCESS_KEY", "marilasecret")

BUCKET = os.environ.get("BUCKET", "taxi")
NAMESPACE = os.environ.get("NAMESPACE", "nyc")
TABLE = os.environ.get("TABLE", "yellow")
DUCKDB_S3_ENDPOINT = os.environ.get("DUCKDB_S3_ENDPOINT", "localhost:9000")

OLLAMA_HOST = os.environ.get("OLLAMA_ENDPOINT", "http://localhost:11434")
# CHAT_PROVIDER: "ollama" (default, local) / "openai" / "gemini".
# When using openai/gemini, the relevant API key env var
# (OPENAI_API_KEY / GEMINI_API_KEY) must be set. Both cloud paths
# go through the `openai` SDK — Gemini exposes an OpenAI-compatible
# endpoint at /v1beta/openai/.
CHAT_PROVIDER = os.environ.get("CHAT_PROVIDER", "ollama").lower()
# Per-provider default model; user can override with CHAT_MODEL or
# /model in the TUI. Picks lean tool-use-capable defaults that don't
# burn dollars on a chat-shaped probe.
_DEFAULT_MODEL = {
    "ollama": "gemma4:latest",
    "openai": "gpt-4o-mini",
    "gemini": "gemini-2.5-flash",
}.get(CHAT_PROVIDER, "gemma4:latest")
CHAT_MODEL = os.environ.get("CHAT_MODEL", _DEFAULT_MODEL)

MAX_TOOL_HOPS = int(os.environ.get("MAX_TOOL_HOPS", "50"))
DEFAULT_ROW_LIMIT = int(os.environ.get("DEFAULT_ROW_LIMIT", "200"))

# Underlying Iceberg table — single source of truth, written by the loader.
TABLE_REF = f'lake."{NAMESPACE}"."{TABLE}"'

# Session-local DuckDB view that the agent + pivot tool actually query.
# Materializes derived dimensions (hour_of_day, day_of_week, …) and
# LEFT-JOINs the TLC zone lookup so pickup_borough / pickup_zone /
# dropoff_borough / dropoff_zone are real columns. Without this view
# the model often hallucinates `SELECT day_of_week FROM lake.nyc.yellow`
# and gets a Binder Error — those names only exist as pivot-builder
# aliases, not as Iceberg columns.
VIEW_REF = "taxi"

# Where to find the zone lookup. load.sh downloads it from TLC into
# the cache dir. The agent reads it directly so the view can JOIN.
CACHE_DIR = os.environ.get(
    "CACHE_DIR",
    os.path.join(
        os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")),
        "marila-taxi",
    ),
)
TAXI_ZONES_CSV = os.environ.get(
    "TAXI_ZONES_CSV", os.path.join(CACHE_DIR, "taxi_zone_lookup.csv")
)


SYSTEM_PROMPT = textwrap.dedent(
    f"""\
    You are a SQL analyst for NYC Yellow Taxi trips, stored in Iceberg
    (DuckDB-attached via marila's `/iceberg/v1/*` proxy).

    # YOU EXECUTE SQL — you do not recommend it
    Your job is to **answer the user's question with data you fetched
    yourself**. You have direct DuckDB access through the `pivot` and
    `run_sql` tools — every call returns real rows. **You MUST call a
    data tool (pivot or run_sql) and use its result before producing
    your final answer.** Never emit a SQL template, never use
    placeholder identifiers like `your_table` / `spaghetti_table` /
    `<table>`, never say "you can run this SQL", never ask the user
    to substitute names. If your answer would be SQL-the-user-runs
    instead of data-you-fetched, you have failed the task — call a
    tool instead.

    # Two ways to reference the data
      - `{VIEW_REF}` — a DuckDB view that **already** materializes
        every pivot dimension (`hour_of_day`, `day_of_week`,
        `pickup_date`, `pickup_month`, `payment_method`,
        `passenger_bucket`, `trip_distance_bucket`) AND LEFT-JOINs
        the TLC zone lookup (`pickup_borough`, `pickup_zone`,
        `dropoff_borough`, `dropoff_zone`). **Use `{VIEW_REF}` for
        every `run_sql`** — every dimension shown in `dimensions` is
        a real column of this view.
      - `{TABLE_REF}` — the raw Iceberg table. Only the canonical TLC
        columns (`tpep_pickup_datetime`, `pulocationid`, etc.).
        Reach for this only if you need a column the view drops.

    # Tools
      - `schema_lookup()` — call FIRST on every new question to see
        the view's columns + the pivot dimensions/measures + 3 sample
        rows. Cheap, has no side effects.
      - `pivot(rows, cols?, measure, where?)` — preferred path for
        "X by Y" / "X per Y" / "compare X across Y" questions. Pass
        dimension + measure *names* (e.g. `rows=["hour_of_day"]`,
        `measure="trip_count"`). The system assembles the SQL.
      - `run_sql(sql)` — raw SQL escape hatch for everything pivot
        can't express: top-N-per-group (`ROW_NUMBER() OVER (...)`,
        `QUALIFY rn <= N`), CTE chains, joins beyond the built-in
        zone JOIN, custom WHERE filters, ad-hoc projections. Query
        `{VIEW_REF}` so dimension columns resolve. Read-only —
        `INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP` are rejected.

    # Recipes for common shapes
      - "top N of X per group Y" → `run_sql` with
        `SELECT … FROM {VIEW_REF} QUALIFY ROW_NUMBER() OVER (PARTITION BY Y ORDER BY X DESC) <= N`.
      - "X by Y and Z" with one measure → `pivot(rows=[Y], cols=[Z],
        measure=X)`.
      - Filter then aggregate → `pivot(..., where="pickup_borough = 'Manhattan'")`.

    # Search rules
      - Always call `schema_lookup` first on a new question.
      - Then call `pivot` or `run_sql` to ACTUALLY compute the answer.
      - Stop after at most 4 tool calls per question. If you can't get
        a clean answer in 4, summarise what you found and ask the user
        to refine.

    # Answer format — IMPORTANT
    The user reads your answers in a terminal TUI that renders
    **GitHub-flavored markdown**. Format every answer accordingly:

      - Start with a one-line summary of what you queried.
      - Render the result as a **Markdown table** built from the rows
        you fetched (NOT from your training knowledge). Cap to ~20
        rows for readability and say "showing top-N of M" if you
        truncated.
      - Wrap column names and identifiers in backticks.
      - Add 1-3 short observation bullets after the table if you spot
        something interesting (peak hour, dominant payment type, etc.).

    Mirror the user's language (German → German, English → English).
    If a tool call returns no rows, say so plainly — don't fabricate.
    """
).strip()


TOOL_DEFINITIONS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "schema_lookup",
            "description": (
                "Return the view's columns + pivot dimension/measure "
                "names. Call AT MOST ONCE per question — the result "
                "doesn't change between hops, calling it again is "
                "wasted budget."
            ),
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "pivot",
            "description": (
                "Run a pivot aggregation. Multiple row and column "
                "dimensions are supported — DuckDB's PIVOT does a "
                "cross-product spread on the columns and a multi-level "
                "GROUP BY on the rows. Returns rows + the assembled SQL."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "rows": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": (
                            "One or more row dimension names (see "
                            "schema_lookup). Examples: "
                            "[\"hour_of_day\"], "
                            "[\"day_of_week\", \"hour_of_day\"]."
                        ),
                        "minItems": 1,
                    },
                    "cols": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": (
                            "Zero or more column dimension names. "
                            "Empty array (or omit) means no pivot, just "
                            "GROUP BY rows. Multiple cols expand "
                            "cross-product (e.g. payment_type × vendor_id "
                            "→ 15 spread columns)."
                        ),
                    },
                    "measure": {
                        "type": "string",
                        "description": "Measure name (see schema_lookup).",
                    },
                    "where": {
                        "type": "string",
                        "description": (
                            "Optional WHERE clause body (without the "
                            "'WHERE' keyword). Passed through to DuckDB."
                        ),
                    },
                },
                "required": ["rows", "measure"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "run_sql",
            "description": (
                "Execute a read-only SELECT against the Iceberg table. "
                "Returns rows (capped at 200) + elapsed ms. Use for "
                "filters / joins / window functions the pivot tool "
                "can't express. INSERT/UPDATE/DELETE/CREATE are rejected."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": (
                            "A single SELECT statement. The table is "
                            f"`{TABLE_REF}`."
                        ),
                    }
                },
                "required": ["sql"],
            },
        },
    },
]


# ---------------------------------------------------------------------------
# Clients
# ---------------------------------------------------------------------------


class _OllamaAdapter:
    """Thin wrapper so `run_turn` can talk to the Ollama SDK and the
    OpenAI SDK through the same `.chat(model=, messages=, tools=)`
    surface, returning Ollama's `{"message": {...}}` envelope."""

    def __init__(self, host: str) -> None:
        self._c = ollama.Client(host=host)

    def chat(self, *, model: str, messages: list, tools: list):
        return self._c.chat(model=model, messages=messages, tools=tools)


class _OpenAIAdapter:
    """Wraps the `openai` SDK. Used for both the OpenAI API itself
    and Gemini's OpenAI-compatible endpoint.

    Tool-use protocol differences vs Ollama we normalize here:
      - response shape: `.choices[0].message` → `{"message": {...}}`
      - assistant `tool_calls`: each entry gets `{id, type, function}`;
        we preserve `id` so the next-turn `tool` response can carry
        `tool_call_id` (OpenAI strict-validates this).
      - tool-call `arguments` come back as a JSON string; the agent
        loop already detects str-args and json.loads them, so no
        change there.
    """

    def __init__(self, *, api_key: str, base_url: Optional[str] = None) -> None:
        from openai import OpenAI
        self._c = OpenAI(api_key=api_key, **({"base_url": base_url} if base_url else {}))

    def chat(self, *, model: str, messages: list, tools: list):
        kwargs: dict[str, Any] = {"model": model, "messages": messages}
        if tools:
            kwargs["tools"] = tools
        resp = self._c.chat.completions.create(**kwargs)
        choice = resp.choices[0]
        msg = choice.message
        normalized_tool_calls = [
            {
                "id": tc.id,
                "type": "function",
                "function": {
                    "name": tc.function.name,
                    "arguments": tc.function.arguments,  # JSON string
                },
            }
            for tc in (getattr(msg, "tool_calls", None) or [])
        ]
        return {
            "message": {
                "role": "assistant",
                "content": msg.content or "",
                "tool_calls": normalized_tool_calls,
            }
        }


def make_chat_client(provider: Optional[str] = None):
    """Factory chosen by CHAT_PROVIDER (env), overridable per-call.
    Raises a friendly error if the chosen provider needs an API key
    that isn't set in the environment."""
    p = (provider or CHAT_PROVIDER).lower()
    if p == "ollama":
        return _OllamaAdapter(host=OLLAMA_HOST)
    if p == "openai":
        key = os.environ.get("OPENAI_API_KEY")
        if not key:
            raise RuntimeError(
                "CHAT_PROVIDER=openai but OPENAI_API_KEY is not set."
            )
        return _OpenAIAdapter(api_key=key)
    if p == "gemini":
        key = os.environ.get("GEMINI_API_KEY") or os.environ.get("GOOGLE_API_KEY")
        if not key:
            raise RuntimeError(
                "CHAT_PROVIDER=gemini but GEMINI_API_KEY (or GOOGLE_API_KEY) is not set."
            )
        return _OpenAIAdapter(
            api_key=key,
            base_url="https://generativelanguage.googleapis.com/v1beta/openai/",
        )
    raise ValueError(
        f"unknown CHAT_PROVIDER={p!r} — expected one of: ollama, openai, gemini"
    )


# Back-compat: the TUI imports `make_ollama_client` by name. Keep the
# name pointing at the generic factory so a CHAT_PROVIDER switch
# Just Works without editing chat.py.
def make_ollama_client():
    return make_chat_client()


def make_duckdb_connection() -> duckdb.DuckDBPyConnection:
    """Open an in-memory DuckDB, install + load iceberg, configure the
    S3 secret pointing at marila's RustFS, and ATTACH the Iceberg
    catalog. Also creates the session-local `taxi` view that materializes
    derived dims + JOINs the TLC zone lookup, plus a `taxi_zones`
    helper table. Reused for every tool call in a session."""
    con = duckdb.connect(":memory:")
    con.execute("INSTALL iceberg; LOAD iceberg;")
    con.execute(
        f"""
        CREATE OR REPLACE SECRET s3_warehouse (
            TYPE s3, PROVIDER config,
            KEY_ID '{MARILA_ACCESS_KEY}', SECRET '{MARILA_SECRET}',
            ENDPOINT '{DUCKDB_S3_ENDPOINT}',
            REGION '{MARILA_REGION}', URL_STYLE 'path', USE_SSL false,
            SCOPE 's3://{BUCKET}/'
        );
        """
    )
    con.execute(
        f"""
        ATTACH '{BUCKET}' AS lake (
            TYPE iceberg,
            ENDPOINT '{MARILA_ENDPOINT}/iceberg',
            AUTHORIZATION_TYPE 'none',
            ACCESS_DELEGATION_MODE 'none'
        );
        """
    )

    # Load the TLC zone lookup if the loader has cached it. The view's
    # JOIN degrades gracefully via LEFT JOIN when the table is missing,
    # so the demo still runs without zones (borough cols just NULL out).
    if os.path.exists(TAXI_ZONES_CSV):
        con.execute(
            f"""
            CREATE OR REPLACE TABLE taxi_zones AS
            SELECT
                CAST("LocationID" AS INT) AS locationid,
                "Borough"     AS borough,
                "Zone"        AS zone,
                service_zone
            FROM read_csv_auto('{TAXI_ZONES_CSV}', header=true);
            """
        )
    else:
        # Empty stub so the view still resolves.
        con.execute(
            "CREATE OR REPLACE TABLE taxi_zones("
            "locationid INT, borough VARCHAR, zone VARCHAR, service_zone VARCHAR);"
        )

    # The enrichment view — every dimension name in DIMENSIONS is a
    # real column here, so `SELECT day_of_week, count(*) FROM taxi
    # GROUP BY 1` just works. Pivots run against this view too.
    con.execute(
        f"""
        CREATE OR REPLACE VIEW {VIEW_REF} AS
        SELECT
            t.*,
            hour(t.tpep_pickup_datetime)                  AS hour_of_day,
            dayname(t.tpep_pickup_datetime)               AS day_of_week,
            date_trunc('day',   t.tpep_pickup_datetime)   AS pickup_date,
            date_trunc('month', t.tpep_pickup_datetime)   AS pickup_month,
            CASE t.payment_type
                WHEN 1 THEN 'Credit card'
                WHEN 2 THEN 'Cash'
                WHEN 3 THEN 'No charge'
                WHEN 4 THEN 'Dispute'
                WHEN 5 THEN 'Unknown'
                WHEN 6 THEN 'Voided'
                ELSE 'Other'
            END                                            AS payment_method,
            CASE
                WHEN t.passenger_count <= 1 THEN '1 pax'
                WHEN t.passenger_count <= 2 THEN '2 pax'
                WHEN t.passenger_count <= 4 THEN '3-4 pax'
                ELSE '5+ pax'
            END                                            AS passenger_bucket,
            CASE
                WHEN t.trip_distance <  1  THEN '<1 mi'
                WHEN t.trip_distance <  3  THEN '1-3 mi'
                WHEN t.trip_distance < 10  THEN '3-10 mi'
                WHEN t.trip_distance < 30  THEN '10-30 mi'
                ELSE '30+ mi'
            END                                            AS trip_distance_bucket,
            pz.borough                                     AS pickup_borough,
            pz.zone                                        AS pickup_zone,
            dz.borough                                     AS dropoff_borough,
            dz.zone                                        AS dropoff_zone
        FROM {TABLE_REF} t
        LEFT JOIN taxi_zones pz ON t.pulocationid = pz.locationid
        LEFT JOIN taxi_zones dz ON t.dolocationid = dz.locationid;
        """
    )
    return con


def preflight(con: duckdb.DuckDBPyConnection) -> tuple[bool, str]:
    """One cheap COUNT — does the table exist and have rows?"""
    try:
        n = con.execute(f"SELECT count(*) FROM {TABLE_REF}").fetchone()[0]
    except Exception as e:  # noqa: BLE001
        return False, str(e)
    return True, f"table {TABLE_REF} has {n:,} rows"


# ---------------------------------------------------------------------------
# Public types
# ---------------------------------------------------------------------------


@dataclass
class AgentEvent:
    """One step of the agent loop — UI subscribers render these."""

    kind: str  # "hop" | "sql" | "sql_result" | "schema" | "synthesis" | "final" | "error"
    data: dict[str, Any]


EventSink = Callable[[AgentEvent], None]


@dataclass
class QueryResult:
    """A run_sql / pivot result, kept in `state.last_queries` so the
    TUI can preview prior runs via the modal."""

    sql: str
    columns: list[str]
    rows: list[tuple]
    row_count: int
    elapsed_ms: float
    error: Optional[str] = None
    # Hierarchy metadata — populated by `tool_pivot` so the TUI's
    # renderer knows to fold the N row-dim columns into a single
    # indented hierarchy column and treat `_g_*` columns as
    # subtotal-level flags. Empty list ⇒ render flat as a generic
    # `run_sql` result.
    row_dim_names: list[str] = field(default_factory=list)
    # Pivot-tool args (populated by `tool_pivot` only — None for
    # `run_sql` results). The TUI uses these to sync the controls
    # pane after the LLM runs a pivot, so the human can pick up
    # from where the LLM left off.
    col_dim_names: list[str] = field(default_factory=list)
    measure: Optional[str] = None
    where: Optional[str] = None


@dataclass
class TablesState:
    chat_model: str = CHAT_MODEL
    default_row_limit: int = DEFAULT_ROW_LIMIT
    messages: list[dict[str, Any]] = field(default_factory=list)
    last_queries: list[QueryResult] = field(default_factory=list)

    def reset(self) -> None:
        self.messages = [{"role": "system", "content": SYSTEM_PROMPT}]
        self.last_queries = []


# ---------------------------------------------------------------------------
# Tool implementations
# ---------------------------------------------------------------------------


READONLY_DENY = (
    "insert",
    "update",
    "delete",
    "create ",
    "drop ",
    "alter ",
    "attach",
    "detach",
    "copy ",
    "pragma",
    "set ",
)


def _reject_writes(sql: str) -> Optional[str]:
    """Return an error string if the SQL looks like a write. Cheap
    keyword check — DuckDB enforces read-only mode via a connection
    flag but only some builds; this guards regardless."""
    lower = sql.lower().lstrip()
    for prefix in READONLY_DENY:
        if lower.startswith(prefix):
            return f"refused: {prefix.strip()!r} is not allowed in this demo"
    return None


def _execute(
    con: duckdb.DuckDBPyConnection,
    sql: str,
    *,
    row_dim_names: Optional[list[str]] = None,
) -> QueryResult:
    if (msg := _reject_writes(sql)) is not None:
        return QueryResult(sql=sql, columns=[], rows=[], row_count=0, elapsed_ms=0.0, error=msg)
    t0 = time.monotonic()
    try:
        cur = con.execute(sql)
        rows = cur.fetchall()
        columns = [d[0] for d in cur.description] if cur.description else []
    except Exception as e:  # noqa: BLE001
        elapsed = (time.monotonic() - t0) * 1000.0
        return QueryResult(sql=sql, columns=[], rows=[], row_count=0, elapsed_ms=elapsed, error=str(e))
    elapsed = (time.monotonic() - t0) * 1000.0
    return QueryResult(
        sql=sql,
        columns=columns,
        rows=rows,
        row_count=len(rows),
        elapsed_ms=elapsed,
        row_dim_names=list(row_dim_names or []),
    )


def tool_schema_lookup(con: duckdb.DuckDBPyConnection) -> dict[str, Any]:
    """Compact schema summary for the LLM. Keep it small — large JSON
    payloads cause smaller tool-use models (gemma4 in particular) to
    drop the tool result from working memory and re-call this tool in
    a loop. Empirically ~2KB stays inside the gemma4 attention window."""
    view_cols = con.execute(f"DESCRIBE {VIEW_REF}").fetchall()
    # Just `name: type` pairs — drop the dict-per-column overhead.
    view_columns = {c[0]: c[1] for c in view_cols}

    # Dimensions: keep `name → description` only. The sql_expr column
    # is irrelevant to the model (the view materializes every dim).
    dim_descs = {d["name"]: d["description"] for d in list_dimensions()}
    measure_descs = {m["name"]: m["description"] for m in list_measures()}

    return {
        "view_to_query": VIEW_REF,
        "view_columns": view_columns,
        "dimension_names": list(dim_descs),
        "dimensions_help": dim_descs,
        "measure_names": list(measure_descs),
        "measures_help": measure_descs,
        "notes": (
            f"Always query `{VIEW_REF}` from run_sql. Every dimension "
            f"name is a real column on the view (already JOINed with "
            f"the TLC zone lookup). Skip calling schema_lookup again — "
            f"you have everything you need."
        ),
    }


def tool_pivot(
    con: duckdb.DuckDBPyConnection,
    *,
    rows: list[str] | str,
    measure: str,
    cols: list[str] | str | None = None,
    where: Optional[str] = None,
    row_limit: int = DEFAULT_ROW_LIMIT,
) -> QueryResult:
    rows_norm = coerce_dim_list(rows)
    cols_norm = coerce_dim_list(cols)
    where_norm = (where or "").strip() or None
    try:
        sql = build_pivot_sql(rows, cols, measure, where, row_limit=row_limit)
    except ValueError as e:
        r = QueryResult(sql="", columns=[], rows=[], row_count=0, elapsed_ms=0.0, error=str(e))
        r.row_dim_names = rows_norm
        r.col_dim_names = cols_norm
        r.measure = measure
        r.where = where_norm
        return r
    r = _execute(con, sql, row_dim_names=rows_norm)
    r.col_dim_names = cols_norm
    r.measure = measure
    r.where = where_norm
    return r


def tool_run_sql(con: duckdb.DuckDBPyConnection, *, sql: str) -> QueryResult:
    return _execute(con, sql)


# ---------------------------------------------------------------------------
# Helpers shared with vector/agent.py
# ---------------------------------------------------------------------------


def _get(obj: Any, attr: str, default: Any = None) -> Any:
    if obj is None:
        return default
    if isinstance(obj, dict):
        return obj.get(attr, default)
    return getattr(obj, attr, default)


def _message_to_dict(msg: Any) -> dict[str, Any]:
    if isinstance(msg, dict):
        return {k: v for k, v in msg.items() if v is not None}
    out: dict[str, Any] = {"role": _get(msg, "role") or "assistant"}
    content = _get(msg, "content")
    if content:
        out["content"] = content
    thinking = _get(msg, "thinking")
    if thinking:
        out["thinking"] = thinking
    tcs = _get(msg, "tool_calls") or []
    if tcs:
        coerced = []
        for tc in tcs:
            fn = _get(tc, "function") or {}
            entry: dict[str, Any] = {
                "function": {
                    "name": _get(fn, "name") or "",
                    "arguments": _get(fn, "arguments") or {},
                }
            }
            # Preserve `id` + `type` for OpenAI/Gemini — they
            # strict-validate tool_call_id on the next-turn `tool`
            # response. Ollama ignores extra fields.
            tc_id = _get(tc, "id")
            if tc_id:
                entry["id"] = tc_id
                entry["type"] = _get(tc, "type") or "function"
            coerced.append(entry)
        out["tool_calls"] = coerced
    return out


# Hallmarks of "model emitted SQL-as-answer instead of executing it".
# We retry once with a corrective system message when we detect any
# of these AND the model didn't call a data tool this turn.
_TEMPLATE_FLAGS = (
    "your_data_table",
    "your_table",
    "spaghetti_table",
    "<table_name>",
    "<table>",
    "table_name_here",
    "actual table name",
    "actual_table_name",
    "please replace",
    "replace this",
    "replace 'your",
    "replace `your",
)


def _looks_like_sql_template(text: str) -> bool:
    low = text.lower()
    return any(flag in low for flag in _TEMPLATE_FLAGS)


def _result_to_tool_payload(r: QueryResult) -> str:
    """Serialise a QueryResult for the model. Truncate rows so we don't
    blow the context window."""
    body: dict[str, Any] = {
        "sql": r.sql,
        "elapsed_ms": round(r.elapsed_ms, 1),
        "row_count": r.row_count,
        "columns": r.columns,
    }
    if r.error is not None:
        body["error"] = r.error
    # Keep at most 50 rows; convert non-JSON-serialisable values to strings.
    cap = 50
    rows: list[Any] = []
    for tup in r.rows[:cap]:
        coerced = []
        for v in tup:
            try:
                json.dumps(v)
                coerced.append(v)
            except TypeError:
                coerced.append(str(v))
        rows.append(coerced)
    body["rows"] = rows
    if r.row_count > cap:
        body["truncated"] = True
    return json.dumps(body, ensure_ascii=False)


# ---------------------------------------------------------------------------
# Agent loop
# ---------------------------------------------------------------------------


def run_turn(
    state: TablesState,
    ollama_client: Any,  # _OllamaAdapter | _OpenAIAdapter (duck-typed)
    duckdb_conn: duckdb.DuckDBPyConnection,
    user_text: str,
    *,
    on_event: Optional[EventSink] = None,
) -> str:
    """One user-input round-trip. Returns the final answer string and
    appends executed-query records to `state.last_queries`."""

    def emit(kind: str, **data: Any) -> None:
        if on_event is not None:
            try:
                on_event(AgentEvent(kind=kind, data=data))
            except Exception:  # noqa: BLE001
                pass

    state.messages.append({"role": "user", "content": user_text})
    final_text = ""
    queries_at_start = len(state.last_queries)

    # Loop-breaker: if the model calls the same tool with the same
    # arguments N times in a row, we treat that as "stuck" and force
    # synthesis. Without this, gemma4 occasionally loops on
    # schema_lookup until the hop budget is exhausted.
    last_tool_signature: Optional[str] = None
    repeat_count = 0
    REPEAT_LIMIT = 2
    # Corrective-retry budget: ONE per turn covering two failure modes
    # that share a root cause ("model produced text but no data"):
    #   (a) SQL-template-as-answer ("SELECT … FROM your_data_table")
    #   (b) thinking-only response (model reasoned but didn't call a
    #       tool — gemma4 in particular drifts here on harder asks).
    # The corrective message is the same in both cases: "STOP, call a
    # tool with the real table name now".
    corrective_retried = False

    for hop in range(MAX_TOOL_HOPS):
        emit("hop", n=hop + 1, total=MAX_TOOL_HOPS, model=state.chat_model)
        try:
            resp = ollama_client.chat(
                model=state.chat_model,
                messages=state.messages,
                tools=TOOL_DEFINITIONS,
            )
        except Exception as e:  # noqa: BLE001
            emit("error", phase="chat", error=str(e))
            return f"(chat error: {e})"

        msg = (
            getattr(resp, "message", None)
            or (resp.get("message") if isinstance(resp, dict) else None)
            or {}
        )
        content = _get(msg, "content") or ""
        thinking = _get(msg, "thinking") or ""
        tool_calls = _get(msg, "tool_calls") or []

        emit(
            "response",
            content_len=len(content),
            thinking_len=len(thinking),
            tool_call_count=len(tool_calls),
        )

        state.messages.append(_message_to_dict(msg))

        if not tool_calls:
            no_data_called = len(state.last_queries) == queries_at_start
            content_stripped = content.strip()
            thinking_stripped = thinking.strip()
            is_template = bool(content_stripped) and _looks_like_sql_template(content_stripped)
            # "thinking-only" = model reasoned but produced no
            # user-facing content and no tool calls. Common gemma4
            # failure mode on multi-step asks.
            is_thinking_only = (not content_stripped) and bool(thinking_stripped)

            if (
                not corrective_retried
                and no_data_called
                and (is_template or is_thinking_only)
            ):
                corrective_retried = True
                phase = "template_detected" if is_template else "thinking_only_no_tools"
                snippet = (content_stripped or thinking_stripped)[:80]
                emit("error", phase=phase, snippet=snippet)
                state.messages.append(
                    {
                        "role": "user",
                        "content": (
                            "Your previous response produced no data. "
                            + (
                                "It was a SQL template with placeholder "
                                "identifiers (your_data_table / "
                                "your_table / <table>). "
                                if is_template
                                else "You reasoned through the plan in your "
                                "`thinking` channel but called no tool and "
                                "emitted no `content`. "
                            )
                            + f"STOP — call the `run_sql` or `pivot` tool "
                            f"RIGHT NOW with the real table name "
                            f"`{VIEW_REF}`. Do not plan, do not recommend "
                            "SQL — execute it. If you do not call a tool "
                            "on your next turn, the user sees no answer."
                        ),
                    }
                )
                continue  # re-enter the loop with the corrective in context

            if content_stripped:
                final_text = content
            elif thinking_stripped:
                # Persistent thinking-only after the retry, OR retry
                # already burned. Don't dump raw thinking — it's
                # internal monologue, not an answer.
                final_text = (
                    "_The model planned a response in its `thinking` "
                    "channel but didn't produce a `content` answer or "
                    "call a tool. This is a known weakness of `gemma4` "
                    "on multi-step questions._\n\n"
                    "Try one of:\n"
                    "- `/reset` and rephrase more concretely "
                    "(e.g. \"pivot rows=`pickup_borough`, "
                    "cols=`payment_method`, measure=`total_revenue`\")\n"
                    "- `/model granite4:latest` (better at tool-use "
                    "discipline)\n"
                    "- `/use N` to fall back on a previous query "
                    "and tweak via controls"
                )
            else:
                final_text = (
                    "(model returned an empty response. Try `/reset` or "
                    "`/model granite4:latest`.)"
                )
            break

        # Detect a tool-call loop: if every tool_call in this hop has
        # the same (name, args) signature as the previous hop's, bump
        # the repeat counter. After REPEAT_LIMIT consecutive identical
        # hops, abort the loop and force synthesis.
        sig_now = json.dumps(
            sorted(
                (
                    _get(_get(c, "function") or {}, "name") or "",
                    json.dumps(_get(_get(c, "function") or {}, "arguments") or {}, sort_keys=True, default=str),
                )
                for c in tool_calls
            )
        )
        if sig_now == last_tool_signature:
            repeat_count += 1
        else:
            repeat_count = 0
            last_tool_signature = sig_now
        if repeat_count >= REPEAT_LIMIT:
            emit("error", phase="loop_break", repeats=repeat_count + 1, signature=sig_now[:120])
            state.messages.append(
                {
                    "role": "user",
                    "content": (
                        f"You have called the same tool with the same "
                        f"arguments {repeat_count + 1} times in a row "
                        f"without making progress. STOP calling tools. "
                        f"Produce the final answer NOW using whatever "
                        f"results you already have in your context. If "
                        f"you have no data, say so plainly — do not "
                        f"call another tool."
                    ),
                }
            )
            try:
                resp = ollama_client.chat(
                    model=state.chat_model, messages=state.messages, tools=[]
                )
            except Exception as e:  # noqa: BLE001
                emit("error", phase="loop_break_synth", error=str(e))
                return f"(loop-break synthesis error: {e})"
            msg = (
                getattr(resp, "message", None)
                or (resp.get("message") if isinstance(resp, dict) else None)
                or {}
            )
            state.messages.append(_message_to_dict(msg))
            content = _get(msg, "content") or ""
            thinking = _get(msg, "thinking") or ""
            final_text = content.strip() or thinking.strip() or (
                "(agent looped on a single tool and the loop-break "
                "synthesis produced no answer either — try `/reset` "
                "and rephrase, or `/model granite4:latest`.)"
            )
            break

        for call in tool_calls:
            fn = _get(call, "function") or {}
            name = _get(fn, "name") or ""
            args = _get(fn, "arguments") or {}
            if isinstance(args, str):
                try:
                    args = json.loads(args)
                except Exception:
                    args = {}
            args = dict(args or {})
            # OpenAI/Gemini emit a `tool_calls[].id` that must be
            # echoed as `tool_call_id` on the corresponding `tool`
            # response message. Ollama ignores the field, so we
            # always include it when present.
            tc_id = _get(call, "id")

            def _tool_msg(name: str, content: str) -> dict[str, Any]:
                m: dict[str, Any] = {"role": "tool", "name": name, "content": content}
                if tc_id:
                    m["tool_call_id"] = tc_id
                return m

            if name == "schema_lookup":
                emit("schema")
                schema = tool_schema_lookup(duckdb_conn)
                state.messages.append(
                    _tool_msg("schema_lookup", json.dumps(schema, ensure_ascii=False))
                )
            elif name == "pivot":
                rows = coerce_dim_list(args.get("rows"))
                cols = coerce_dim_list(args.get("cols"))
                measure = str(args.get("measure") or "")
                where = args.get("where") or None
                emit("sql", tool="pivot", rows=rows, cols=cols, measure=measure, where=where)
                r = tool_pivot(
                    duckdb_conn,
                    rows=rows,
                    measure=measure,
                    cols=cols,
                    where=where,
                    row_limit=state.default_row_limit,
                )
                state.last_queries.append(r)
                emit(
                    "sql_result",
                    sql=r.sql,
                    row_count=r.row_count,
                    elapsed_ms=round(r.elapsed_ms, 1),
                    error=r.error,
                )
                state.messages.append(_tool_msg("pivot", _result_to_tool_payload(r)))
            elif name == "run_sql":
                sql = str(args.get("sql") or "")
                emit("sql", tool="run_sql", sql_preview=sql[:160])
                r = tool_run_sql(duckdb_conn, sql=sql)
                state.last_queries.append(r)
                emit(
                    "sql_result",
                    sql=sql,
                    row_count=r.row_count,
                    elapsed_ms=round(r.elapsed_ms, 1),
                    error=r.error,
                )
                state.messages.append(_tool_msg("run_sql", _result_to_tool_payload(r)))
            else:
                emit("error", phase="tool_dispatch", name=name)
                state.messages.append(
                    _tool_msg(name or "unknown", json.dumps({"error": f"unknown tool {name!r}"}))
                )
    else:
        # Tool-budget exhausted — force synthesis with tools off.
        emit("synthesis", query_count=len(state.last_queries))
        state.messages.append(
            {
                "role": "user",
                "content": (
                    "You have exhausted your tool budget. Produce the "
                    "final answer NOW based on the queries already in "
                    "your context. Render the most relevant result as a "
                    "Markdown table. If the evidence is incomplete, say "
                    "so plainly — do not invent details."
                ),
            }
        )
        try:
            resp = ollama_client.chat(
                model=state.chat_model, messages=state.messages, tools=[]
            )
        except Exception as e:  # noqa: BLE001
            emit("error", phase="synthesis", error=str(e))
            return f"(synthesis chat error: {e})"
        msg = (
            getattr(resp, "message", None)
            or (resp.get("message") if isinstance(resp, dict) else None)
            or {}
        )
        state.messages.append(_message_to_dict(msg))
        content = _get(msg, "content") or ""
        thinking = _get(msg, "thinking") or ""
        if content.strip():
            final_text = content
        elif thinking.strip():
            final_text = (
                "(synthesis turn produced no `content` — falling back to "
                "thinking)\n\n" + thinking
            )
        else:
            final_text = (
                "(agent burned its tool budget AND the synthesis turn "
                "produced an empty response — try `/reset` and a more "
                "specific question.)"
            )

    emit("final", length=len(final_text), query_count=len(state.last_queries))
    return final_text


def execute_pivot_direct(
    state: TablesState,
    duckdb_conn: duckdb.DuckDBPyConnection,
    *,
    rows: list[str] | str,
    cols: list[str] | str | None,
    measure: str,
    where: Optional[str] = None,
) -> QueryResult:
    """Bypass the LLM — used by the controls pane's F5 "run" button.
    Same code path the agent's `pivot` tool uses."""
    r = tool_pivot(
        duckdb_conn,
        rows=rows,
        measure=measure,
        cols=cols,
        where=where,
        row_limit=state.default_row_limit,
    )
    state.last_queries.append(r)
    return r
