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
    DIMENSIONS,
    MEASURES,
    build_pivot_sql,
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
CHAT_MODEL = os.environ.get("CHAT_MODEL", "gemma4:latest")

MAX_TOOL_HOPS = int(os.environ.get("MAX_TOOL_HOPS", "50"))
DEFAULT_ROW_LIMIT = int(os.environ.get("DEFAULT_ROW_LIMIT", "200"))

TABLE_REF = f'lake."{NAMESPACE}"."{TABLE}"'


SYSTEM_PROMPT = textwrap.dedent(
    f"""\
    You are a SQL analyst for an Iceberg table of NYC Yellow Taxi
    trips. The table is `{TABLE_REF}` (DuckDB-attached via marila's
    Iceberg REST proxy). Answer the user's question by calling tools.

    # Tools
      - `schema_lookup()` — call FIRST to see columns, types, dimensions,
        and measures available for `pivot`. Returns small sample rows.
      - `pivot(rows, cols?, measure, where?)` — preferred path. Pass
        dimension + measure *names* (e.g. `rows="hour_of_day"`,
        `measure="trip_count"`). The system assembles the SQL.
      - `run_sql(sql)` — raw SQL escape hatch. Use for filters / windows
        / joins that the pivot tool can't express. Read-only;
        `INSERT`/`UPDATE`/`DELETE` are rejected.

    # Search rules
      - Always call `schema_lookup` first on a new question.
      - Prefer `pivot` over `run_sql` when the question is "X by Y" /
        "X per Y" / "compare X across Y" — that's exactly what pivots
        are for, and you avoid hand-writing CASE/GROUP-BY.
      - Stop after at most 3 tool calls per question. If you can't get
        a clean answer in 3, summarise what you found and ask the user
        to refine.

    # Answer format — IMPORTANT
    The user reads your answers in a terminal TUI that renders
    **GitHub-flavored markdown**. Format every answer accordingly:

      - Start with a one-line summary of what you queried.
      - Render the result as a **Markdown table** (the row + col
        dimensions you used). Cap to ~20 rows for readability and
        say "showing top-N of M" if you truncated.
      - Wrap column names and identifiers in backticks.
      - Add 1-3 short observation bullets after the table if you spot
        something interesting (peak hour, dominant payment type, etc.).

    Mirror the user's language (German → German, English → English).
    If a search returns nothing useful, say so plainly.
    """
).strip()


TOOL_DEFINITIONS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "schema_lookup",
            "description": (
                "Return the Iceberg table's column list (with DuckDB types), "
                "available pivot dimensions, available pivot measures, and "
                "the first 3 sample rows."
            ),
            "parameters": {"type": "object", "properties": {}},
        },
    },
    {
        "type": "function",
        "function": {
            "name": "pivot",
            "description": (
                "Run a pivot aggregation. Dimensions and measures are "
                "selected by name; see `schema_lookup` for the catalog. "
                "Returns rows + the assembled SQL for transparency."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "rows": {
                        "type": "string",
                        "description": "Row dimension name (see schema_lookup).",
                    },
                    "cols": {
                        "type": "string",
                        "description": (
                            "Optional column dimension name. Omit or set "
                            "to null for a simple 1-dim GROUP BY."
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


def make_ollama_client() -> ollama.Client:
    return ollama.Client(host=OLLAMA_HOST)


def make_duckdb_connection() -> duckdb.DuckDBPyConnection:
    """Open an in-memory DuckDB, install + load iceberg, configure the
    S3 secret pointing at marila's RustFS, and ATTACH the Iceberg
    catalog. Reused for every tool call in a session."""
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


def _execute(con: duckdb.DuckDBPyConnection, sql: str) -> QueryResult:
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
    )


def tool_schema_lookup(con: duckdb.DuckDBPyConnection) -> dict[str, Any]:
    cols = con.execute(f"DESCRIBE {TABLE_REF}").fetchall()
    cols_out = [{"name": c[0], "type": c[1]} for c in cols]

    sample_cur = con.execute(f"SELECT * FROM {TABLE_REF} LIMIT 3")
    sample_cols = [d[0] for d in sample_cur.description]
    sample_rows = [dict(zip(sample_cols, r)) for r in sample_cur.fetchall()]
    # JSON-encode any datetime/decimal so it round-trips cleanly.
    def _sanitize(v: Any) -> Any:
        try:
            json.dumps(v)
            return v
        except TypeError:
            return str(v)
    sample_rows = [{k: _sanitize(v) for k, v in r.items()} for r in sample_rows]

    return {
        "table": TABLE_REF,
        "columns": cols_out,
        "dimensions": list_dimensions(),
        "measures": list_measures(),
        "sample_rows": sample_rows,
    }


def tool_pivot(
    con: duckdb.DuckDBPyConnection,
    *,
    rows: str,
    measure: str,
    cols: Optional[str] = None,
    where: Optional[str] = None,
    row_limit: int = DEFAULT_ROW_LIMIT,
) -> QueryResult:
    try:
        sql = build_pivot_sql(rows, cols, measure, where, row_limit=row_limit)
    except ValueError as e:
        return QueryResult(sql="", columns=[], rows=[], row_count=0, elapsed_ms=0.0, error=str(e))
    return _execute(con, sql)


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
            coerced.append(
                {
                    "function": {
                        "name": _get(fn, "name") or "",
                        "arguments": _get(fn, "arguments") or {},
                    }
                }
            )
        out["tool_calls"] = coerced
    return out


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
    ollama_client: ollama.Client,
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
            if content.strip():
                final_text = content
            elif thinking.strip():
                final_text = (
                    "(model emitted no `content` channel — falling back to "
                    "its `thinking` channel)\n\n" + thinking
                )
            else:
                final_text = (
                    "(model returned an empty response. Try `/reset` or "
                    "`/model granite4:latest`.)"
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

            if name == "schema_lookup":
                emit("schema")
                schema = tool_schema_lookup(duckdb_conn)
                state.messages.append(
                    {
                        "role": "tool",
                        "name": "schema_lookup",
                        "content": json.dumps(schema, ensure_ascii=False),
                    }
                )
            elif name == "pivot":
                rows = str(args.get("rows") or "")
                cols = args.get("cols") or None
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
                state.messages.append(
                    {
                        "role": "tool",
                        "name": "pivot",
                        "content": _result_to_tool_payload(r),
                    }
                )
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
                state.messages.append(
                    {
                        "role": "tool",
                        "name": "run_sql",
                        "content": _result_to_tool_payload(r),
                    }
                )
            else:
                emit("error", phase="tool_dispatch", name=name)
                state.messages.append(
                    {
                        "role": "tool",
                        "name": name or "unknown",
                        "content": json.dumps({"error": f"unknown tool {name!r}"}),
                    }
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
    rows: str,
    cols: Optional[str],
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
