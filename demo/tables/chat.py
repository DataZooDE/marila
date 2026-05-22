"""Textual TUI for the marila s3-tables / Iceberg slice-dice demo.

Mirrors `demo/vector/chat.py` so muscle-memory transfers: same Splitter,
same F-key bindings, same modal-on-`p`, same input history. Differences:

  - sources pane → **controls pane** (Select widgets for rows/cols/measure,
    a Where input, F5 to run the pivot bypassing the LLM)
  - last_sources → **last_queries** (each SQL query the agent ran)
  - the preview modal shows the full SQL + result table, not a chunk
    snippet

Run:
    cd demo && uv run python -m tables.chat
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from rich.markdown import Markdown as RichMarkdown
from rich.rule import Rule
from rich.table import Table as RichTable
from rich.text import Text

from textual import work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical
from textual.message import Message
from textual.reactive import reactive
from textual.screen import ModalScreen
from textual.widgets import (
    Footer,
    Header,
    Input,
    Label,
    ListItem,
    ListView,
    RichLog,
    Select,
    Static,
)

from shared.tui_widgets import Splitter
from shared.pivot_sql import DIMENSIONS, MEASURES, build_pivot_sql

from tables.agent import (
    AgentEvent,
    BUCKET,
    CHAT_MODEL,
    CHAT_PROVIDER,
    DEFAULT_ROW_LIMIT,
    NAMESPACE,
    OLLAMA_HOST,
    PROVIDER_DEFAULT_MODELS,
    QueryResult,
    TABLE,
    TablesState,
    execute_pivot_direct,
    make_chat_client,
    make_duckdb_connection,
    make_ollama_client,
    preflight,
    run_turn,
    tool_run_sql,
)


# ---------------------------------------------------------------------------
# Inter-thread messages
# ---------------------------------------------------------------------------


class AgentEventMessage(Message):
    def __init__(self, event: AgentEvent) -> None:
        super().__init__()
        self.event = event


class AssistantAnswer(Message):
    def __init__(self, text: str, queries: list[QueryResult]) -> None:
        super().__init__()
        self.text = text
        self.queries = queries


class AgentError(Message):
    def __init__(self, err: str) -> None:
        super().__init__()
        self.err = err


class DirectPivotResult(Message):
    """Posted by the controls-pane F5 path so the chat pane updates."""

    def __init__(self, label: str, result: QueryResult) -> None:
        super().__init__()
        self.label = label
        self.result = result


# ---------------------------------------------------------------------------
# Query-detail modal (analogue of vector's SourcePreview)
# ---------------------------------------------------------------------------


class QueryDetail(ModalScreen[None]):
    BINDINGS = [
        Binding("escape", "dismiss(None)", "Close"),
        Binding("q", "dismiss(None)", "Close"),
    ]
    DEFAULT_CSS = """
    QueryDetail { align: center middle; }
    #qd-box {
        width: 90%;
        height: 90%;
        background: $surface;
        border: thick $accent;
        padding: 1 2;
    }
    #qd-title {
        text-style: bold;
        color: $accent;
    }
    #qd-meta {
        color: $text-muted;
        margin-bottom: 1;
    }
    #qd-body {
        background: $background;
        padding: 1;
        height: 1fr;
        border: round $primary;
    }
    """

    def __init__(self, result: QueryResult) -> None:
        super().__init__()
        self.result = result

    def compose(self) -> ComposeResult:
        r = self.result
        title = "query failed" if r.error else f"query — {r.row_count} rows"
        with Container(id="qd-box"):
            yield Label(title, id="qd-title")
            yield Label(f"elapsed {r.elapsed_ms:.1f} ms · {len(r.columns)} cols", id="qd-meta")
            log = RichLog(id="qd-body", wrap=True, markup=False, highlight=False)
            log.write(Text("SQL:", style="bold"))
            log.write(Text(r.sql))
            log.write(Text(""))
            if r.error:
                log.write(Text("ERROR:", style="bold red"))
                log.write(Text(r.error, style="red"))
            else:
                log.write(_render_result_table(r))
            yield log
            yield Label("Esc / q to close", classes="hint")


def _fmt_value(v: Any) -> str:
    if v is None:
        return ""
    if isinstance(v, bool):
        return str(v)
    if isinstance(v, int):
        return f"{v:,}"
    if isinstance(v, float):
        # 2 decimals + thousand separators; trim trailing .00 for clean ints
        s = f"{v:,.2f}"
        return s[:-3] if s.endswith(".00") else s
    return str(v)


def _try_infer_pivot_from_result(
    q: QueryResult,
) -> Optional[tuple[list[str], list[str], str]]:
    """If a `run_sql` result LOOKS pivot-shaped, back-derive
    `(rows, cols, measure)` so the controls pane can still sync.

    A column list of the form `[dim1, dim2, …, dimN, measure]` where
    every dim name is in DIMENSIONS and the trailing column is in
    MEASURES counts as pivot-shaped. We don't try to infer cols
    (would need spread detection) — best-effort, returns cols=[].
    Returns None when the result doesn't fit (CTE projections,
    custom aliases, etc.) — controls stay where they were.
    """
    if q.measure is not None or q.error:
        return None
    if not q.columns or len(q.columns) < 2:
        return None
    *maybe_dims, maybe_measure = q.columns
    if not maybe_dims:
        return None
    if any(c not in DIMENSIONS for c in maybe_dims):
        return None
    if maybe_measure not in MEASURES:
        return None
    return (list(maybe_dims), [], maybe_measure)


def _render_result_table(
    r: QueryResult,
    *,
    max_rows: int = 200,
    max_cols: Optional[int] = None,
) -> RichTable:
    """Render a QueryResult.

    If `row_dim_names` is set (pivot path), fold the N row-dim columns
    into a single indented "hierarchy" column with TOTAL row at top,
    subtotals bold, leaves plain. The `_g_*` GROUPING flags drive the
    level detection; they're never displayed.

    If `row_dim_names` is empty (raw `/sql` or schema lookup), fall
    back to a flat one-column-per-result-column render.

    `max_cols` caps the visible columns and adds a "+N more" stub on
    each row. Wide pivots (e.g. day_of_week × hour_of_day = 168 spread
    cols) otherwise collapse to a hatch pattern when squeezed into the
    chat-pane width.
    """
    title = (
        f"{r.row_count} row{'s' if r.row_count != 1 else ''} "
        f"({r.elapsed_ms:.0f} ms)"
    )

    # ── Flat path: no hierarchy info ──
    if not r.row_dim_names:
        cols_to_show = list(r.columns)
        n_dropped = 0
        if max_cols is not None and len(cols_to_show) > max_cols:
            cols_to_show = cols_to_show[: max(1, max_cols - 1)]
            n_dropped = len(r.columns) - len(cols_to_show)
        t = RichTable(title=title, show_header=True, header_style="bold cyan")
        for c in cols_to_show:
            t.add_column(str(c), justify="right")
        if n_dropped > 0:
            t.add_column(f"+{n_dropped} more")
        keep = len(cols_to_show)
        for row in r.rows[:max_rows]:
            cells = [_fmt_value(v) for v in row[:keep]]
            if n_dropped > 0:
                cells.append("…")
            t.add_row(*cells)
        caps = []
        if r.row_count > max_rows:
            caps.append(f"first {max_rows} of {r.row_count} rows")
        if n_dropped > 0:
            caps.append(f"first {keep} of {len(r.columns)} cols")
        if caps:
            t.caption = "showing " + ", ".join(caps)
        return t

    # ── Hierarchical path: pivot with rollup ──
    # Render one column per row dim (so dim values are unambiguous —
    # `hour=0` and `vendor=1` are obviously different cells) and leave
    # the cell BLANK on a subtotal row when its dim was rolled up.
    # The leftmost dim column doubles as the TOTAL-row label cell.
    n = len(r.row_dim_names)
    try:
        dim_idx = [r.columns.index(name) for name in r.row_dim_names]
        g_idx = [r.columns.index(f"_g_{name}") for name in r.row_dim_names]
    except ValueError:
        return _render_result_table(
            QueryResult(
                sql=r.sql, columns=r.columns, rows=r.rows,
                row_count=r.row_count, elapsed_ms=r.elapsed_ms,
                error=r.error,
            ),
            max_rows=max_rows,
            max_cols=max_cols,
        )
    hidden_for_display = set(g_idx)  # never show GROUPING flags
    measure_idx = [
        i for i in range(len(r.columns))
        if i not in hidden_for_display and i not in dim_idx
    ]
    measure_names = [r.columns[i] for i in measure_idx]

    # Cap the SPREAD (measure) columns; never drop dim columns, since
    # they carry the hierarchy labels.
    spread_dropped = 0
    if max_cols is not None:
        budget = max(2, max_cols - n)  # leave room for at least 2 measures
        if len(measure_idx) > budget:
            spread_dropped = len(measure_idx) - (budget - 1)  # save 1 col for the +N stub
            measure_idx = measure_idx[: budget - 1]
            measure_names = measure_names[: budget - 1]

    t = RichTable(title=title, show_header=True, header_style="bold cyan")
    # One column per row dim — clear, unambiguous.
    for name in r.row_dim_names:
        t.add_column(name, no_wrap=True)
    for name in measure_names:
        t.add_column(name, justify="right")
    if spread_dropped > 0:
        t.add_column(f"+{spread_dropped} more")

    for row in r.rows[:max_rows]:
        g_flags = [row[i] for i in g_idx]
        level = sum(1 for g in g_flags if g == 0)

        # One cell per dim — empty if rolled up, value otherwise.
        dim_cells = []
        for di, gi in zip(dim_idx, g_idx):
            if row[gi] == 1:
                dim_cells.append("")  # rolled up
            else:
                v = row[di]
                dim_cells.append("(NULL)" if v is None else str(v))

        # Style + special label
        if level == 0:
            # Grand total — every dim cell is blank; commandeer the
            # leftmost one to carry the TOTAL marker.
            dim_cells[0] = "TOTAL"
            style: Optional[str] = "bold magenta"
        elif level < n:
            style = "bold"
        else:
            style = None

        measure_cells = [_fmt_value(row[i]) for i in measure_idx]
        if spread_dropped > 0:
            measure_cells.append("…")
        t.add_row(*dim_cells, *measure_cells, style=style)

    caps = []
    if r.row_count > max_rows:
        caps.append(f"first {max_rows} of {r.row_count} rows")
    if spread_dropped > 0:
        caps.append(f"first {len(measure_idx)} of {len(measure_idx) + spread_dropped} measure cols")
    if caps:
        t.caption = "showing " + ", ".join(caps)
    return t


# ---------------------------------------------------------------------------
# Main App
# ---------------------------------------------------------------------------


class TablesChat(App[None]):
    CHAT_WIDTH_MIN, CHAT_WIDTH_MAX = 30, 85
    VERBOSE_HEIGHT_MIN, VERBOSE_HEIGHT_MAX = 15, 85

    CSS = """
    Screen { layers: base modal; }
    #main {
        layout: horizontal;
        height: 1fr;
    }
    #chat-pane {
        width: 66%;
        border: round $primary;
    }
    #side-pane {
        width: 1fr;
        layout: vertical;
    }
    #verbose-pane {
        height: 50%;
        border: round $secondary;
    }
    #controls-pane {
        height: 1fr;
        border: round $secondary;
    }
    #controls-form {
        padding: 0 1;
    }
    #controls-form Static {
        margin-top: 1;
    }
    #controls-form Select, #controls-form ListView {
        margin-bottom: 0;
    }
    #dim-list {
        height: 11;
        border: round $primary;
    }
    #rows-list, #cols-list {
        height: 5;
        border: round $accent 50%;
    }
    #where-input {
        margin-top: 1;
    }
    #chat-log, #verbose-log {
        height: 1fr;
    }
    Input {
        dock: bottom;
    }
    .pane-title {
        text-style: bold;
        background: $secondary;
        color: $text;
        padding: 0 1;
    }
    .hint {
        color: $text-muted;
        text-style: italic;
    }
    """

    BINDINGS = [
        Binding("ctrl+c", "quit", "Quit"),
        Binding("ctrl+r", "reset", "Reset chat"),
        Binding("ctrl+l", "clear_chat", "Clear pane"),
        Binding("f2", "focus_input", "→ input"),
        Binding("f3", "focus_chat", "→ chat (scroll)"),
        Binding("f4", "focus_controls", "→ controls"),
        Binding("f5", "run_pivot", "Run pivot"),
        Binding("f6", "shrink_chat", "Chat ←"),
        Binding("f7", "grow_chat", "Chat →"),
        Binding("f8", "shrink_verbose", "Verbose ↑"),
        Binding("f9", "grow_verbose", "Verbose ↓"),
    ]

    busy: reactive[bool] = reactive(False)
    chat_width_pct: reactive[int] = reactive(66)
    verbose_height_pct: reactive[int] = reactive(50)

    def __init__(self) -> None:
        super().__init__()
        self.state = TablesState()
        self.state.reset()
        # Active chat provider — startup value comes from CHAT_PROVIDER
        # env, mutable mid-session via the /provider slash command.
        self.current_provider: str = CHAT_PROVIDER
        self.oc = make_ollama_client()
        self.con = make_duckdb_connection()
        self.history: list[str] = []
        self.history_idx: int = 0
        self.input_draft: str = ""
        # Pivot-controls state: ordered lists, mutated by r/c/d/J/K key
        # handlers. The order is the SQL row/col order, which becomes
        # the hierarchy depth.
        self.rows_order: list[str] = ["hour_of_day"]
        self.cols_order: list[str] = []
        # Queries the agent ran during the latest turn, in run order.
        # Digit shortcuts (1-9) and `/show N` open them in the
        # detail modal so the user can drill into the SQL + data.
        self.current_turn_queries: list[QueryResult] = []

    # ----- layout -----

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Horizontal(id="main"):
            with Vertical(id="chat-pane"):
                yield Static(
                    "chat  (F3 focus · then ↑↓ / PgUp PgDn / wheel scroll)",
                    classes="pane-title",
                )
                yield RichLog(
                    id="chat-log",
                    wrap=True,
                    markup=True,
                    highlight=False,
                    auto_scroll=True,
                )
            yield Splitter(
                orientation="vertical",
                target_id="chat-pane",
                app_attr="chat_width_pct",
                min_pct=self.CHAT_WIDTH_MIN,
                max_pct=self.CHAT_WIDTH_MAX,
                widget_id="vsplit",
            )
            with Vertical(id="side-pane"):
                with Vertical(id="verbose-pane"):
                    yield Static("hops · agent verbose", classes="pane-title")
                    yield RichLog(
                        id="verbose-log",
                        wrap=True,
                        markup=True,
                        highlight=False,
                        max_lines=2000,
                        auto_scroll=True,
                    )
                yield Splitter(
                    orientation="horizontal",
                    target_id="verbose-pane",
                    app_attr="verbose_height_pct",
                    min_pct=self.VERBOSE_HEIGHT_MIN,
                    max_pct=self.VERBOSE_HEIGHT_MAX,
                    widget_id="hsplit",
                )
                with Vertical(id="controls-pane"):
                    yield Static(
                        "pivot controls  (F4 focus · F5 run)",
                        classes="pane-title",
                    )
                    meas_opts = [(m.name, m.name) for m in MEASURES.values()]
                    with Vertical(id="controls-form"):
                        yield Static(
                            "all dimensions  [r] add to rows · [c] add to cols",
                            classes="hint",
                        )
                        yield ListView(
                            *[
                                ListItem(Label(d.name), name=d.name)
                                for d in DIMENSIONS.values()
                            ],
                            id="dim-list",
                        )
                        yield Static(
                            "rows  [j/k] nav · [J/K] reorder · [d/del] remove",
                            classes="hint",
                        )
                        yield ListView(id="rows-list")
                        yield Static(
                            "cols  [j/k] nav · [J/K] reorder · [d/del] remove",
                            classes="hint",
                        )
                        yield ListView(id="cols-list")
                        yield Static("measure:", classes="hint")
                        yield Select(
                            options=meas_opts,
                            value="trip_count",
                            allow_blank=False,
                            id="sel-measure",
                        )
                        yield Input(
                            placeholder="WHERE … (optional, no leading WHERE)",
                            id="where-input",
                        )
                        yield Static("[F5] run pivot", classes="hint")
        yield Input(
            placeholder="ask a question or /sql ... or /help (↑↓ history · F4 controls · F5 run)",
            id="question",
        )
        yield Footer()

    # ----- lifecycle -----

    def on_mount(self) -> None:
        self.title = "marila — tables pivot chat"
        self.sub_title = f"{BUCKET}/{NAMESPACE}/{TABLE}"
        chat = self.query_one("#chat-log", RichLog)
        chat.write(
            f"[dim]chat: {self.state.chat_model}   ollama: {OLLAMA_HOST}[/dim]"
        )
        verbose = self.query_one("#verbose-log", RichLog)
        verbose.write("[dim]waiting for first question…[/dim]")

        ok, msg = preflight(self.con)
        if ok:
            chat.write(f"[green]✓ {msg}[/green]")
        else:
            chat.write(
                f"[red]✗ could not query {NAMESPACE}.{TABLE}: {msg}[/red]\n"
                f"[dim]Run `bash demo/tables/load.sh` first.[/dim]"
            )

        self._refresh_pivot_lists()
        self.query_one("#question", Input).focus()

    # ----- pivot-controls helpers -----

    def _refresh_pivot_lists(self) -> None:
        """Rebuild #rows-list and #cols-list to mirror the current
        `self.rows_order` / `self.cols_order`. Numbers each entry so
        the hierarchy depth is visually obvious."""
        for which, order in (("rows", self.rows_order), ("cols", self.cols_order)):
            lv = self.query_one(f"#{which}-list", ListView)
            current_idx = lv.index if lv.index is not None else 0
            lv.clear()
            for i, name in enumerate(order, start=1):
                lv.append(ListItem(Label(f"{i}. {name}"), name=name))
            if order:
                # Try to preserve highlight position.
                lv.index = min(current_idx, len(order) - 1)

    def _add_to_rows(self, name: str) -> None:
        if name in self.cols_order:
            self._chat_note(f"[yellow]{name!r} is already in cols — remove it first[/yellow]")
            return
        if name not in self.rows_order:
            self.rows_order.append(name)
            self._refresh_pivot_lists()

    def _add_to_cols(self, name: str) -> None:
        if name in self.rows_order:
            self._chat_note(f"[yellow]{name!r} is already in rows — remove it first[/yellow]")
            return
        if name not in self.cols_order:
            self.cols_order.append(name)
            self._refresh_pivot_lists()

    def _remove_from(self, which: str, idx: int) -> None:
        order = self.rows_order if which == "rows" else self.cols_order
        if 0 <= idx < len(order):
            del order[idx]
            self._refresh_pivot_lists()

    def _reorder_within(self, which: str, idx: int, delta: int) -> None:
        order = self.rows_order if which == "rows" else self.cols_order
        new = idx + delta
        if not (0 <= idx < len(order) and 0 <= new < len(order)):
            return
        order[idx], order[new] = order[new], order[idx]
        self._refresh_pivot_lists()
        lv = self.query_one(f"#{which}-list", ListView)
        lv.index = new

    def _load_query_into_controls(self, idx: int) -> None:
        """Pull query #idx's pivot args (explicit or inferred) into
        the controls pane. Lets the user follow up on any of the
        per-turn queries, not just the latest."""
        if not (0 <= idx < len(self.current_turn_queries)):
            self._chat_note(
                f"[red]no query #{idx + 1} — this turn ran "
                f"{len(self.current_turn_queries)} queries[/red]"
            )
            return
        q = self.current_turn_queries[idx]
        if q.measure is not None:
            self._sync_controls_from_pivot(
                q.row_dim_names, q.col_dim_names, q.measure, q.where
            )
            self._chat_note(
                f"[dim cyan]→ loaded query [{idx + 1}] (explicit pivot) "
                f"into controls — F5 to rerun, or edit dims first[/dim cyan]"
            )
            return
        inferred = _try_infer_pivot_from_result(q)
        if inferred is not None:
            rows, cols, measure = inferred
            self._sync_controls_from_pivot(rows, cols, measure, None)
            self._chat_note(
                f"[dim cyan]→ loaded query [{idx + 1}] (inferred from "
                f"run_sql shape) — F5 to rerun as pivot, or edit first[/dim cyan]"
            )
            return
        self._chat_note(
            f"[yellow]query [{idx + 1}] isn't pivot-shaped — its columns "
            f"don't all match registered dimensions/measures. Can't "
            f"load into controls.[/yellow]"
        )

    def _sync_controls_from_pivot(
        self,
        rows: list[str],
        cols: list[str],
        measure: str,
        where: Optional[str],
    ) -> None:
        """Mirror a pivot the LLM just ran into the controls pane.
        Either side (human via F5 or LLM via the `pivot` tool) leaves
        the controls reflecting whatever produced the latest result —
        one canonical pivot state, two input modes."""
        # Drop dims we don't recognise (LLM might pass an alias).
        valid_rows = [r for r in rows if r in DIMENSIONS]
        valid_cols = [c for c in cols if c in DIMENSIONS]
        self.rows_order = list(valid_rows) or list(self.rows_order)
        self.cols_order = list(valid_cols)
        self._refresh_pivot_lists()
        try:
            sel = self.query_one("#sel-measure", Select)
            if measure in MEASURES:
                sel.value = measure
        except Exception:  # noqa: BLE001
            pass
        try:
            inp = self.query_one("#where-input", Input)
            inp.value = where or ""
        except Exception:  # noqa: BLE001
            pass

    # ----- bindings -----

    def action_reset(self) -> None:
        self.state.reset()
        self.query_one("#chat-log", RichLog).write(
            "\n[dim italic]— conversation reset —[/dim italic]\n"
        )
        self.query_one("#verbose-log", RichLog).clear()

    def action_clear_chat(self) -> None:
        self.query_one("#chat-log", RichLog).clear()

    def action_focus_input(self) -> None:
        self.query_one("#question", Input).focus()

    def action_focus_chat(self) -> None:
        chat = self.query_one("#chat-log", RichLog)
        chat.can_focus = True
        chat.focus()

    def action_focus_controls(self) -> None:
        self.query_one("#dim-list", ListView).focus()

    def action_shrink_chat(self) -> None:
        self.chat_width_pct = max(self.CHAT_WIDTH_MIN, self.chat_width_pct - 5)
        self._apply_layout()

    def action_grow_chat(self) -> None:
        self.chat_width_pct = min(self.CHAT_WIDTH_MAX, self.chat_width_pct + 5)
        self._apply_layout()

    def action_shrink_verbose(self) -> None:
        self.verbose_height_pct = max(
            self.VERBOSE_HEIGHT_MIN, self.verbose_height_pct - 10
        )
        self._apply_layout()

    def action_grow_verbose(self) -> None:
        self.verbose_height_pct = min(
            self.VERBOSE_HEIGHT_MAX, self.verbose_height_pct + 10
        )
        self._apply_layout()

    def _apply_layout(self) -> None:
        try:
            chat_pane = self.query_one("#chat-pane")
            verbose_pane = self.query_one("#verbose-pane")
        except Exception:  # noqa: BLE001
            return
        chat_pane.styles.width = f"{self.chat_width_pct}%"
        verbose_pane.styles.height = f"{self.verbose_height_pct}%"

    # ----- input + history -----

    def on_input_submitted(self, event: Input.Submitted) -> None:
        if event.input.id != "question":
            return
        text = event.value.strip()
        if not text:
            return
        if self.busy:
            self._chat_note("[yellow]busy — wait for the current turn to finish[/yellow]")
            return
        event.input.value = ""
        self.input_draft = ""
        if not self.history or self.history[-1] != text:
            self.history.append(text)
        self.history_idx = len(self.history)

        if text.startswith("/"):
            self._handle_slash(text)
            return

        self._chat_note(f"\n[bold green]you>[/bold green] {text}")
        self.busy = True
        self.handle_turn(text)

    # ----- agent worker -----

    @work(thread=True, exclusive=True)
    def handle_turn(self, question: str) -> None:
        def emit(ev: AgentEvent) -> None:
            self.post_message(AgentEventMessage(ev))

        # Snapshot the queries-log length BEFORE the agent runs so we
        # can hand the AssistantAnswer message the slice of queries
        # actually produced by this turn (not the cumulative log).
        before = len(self.state.last_queries)
        try:
            answer = run_turn(
                self.state, self.oc, self.con, question, on_event=emit
            )
            per_turn = list(self.state.last_queries[before:])
            self.post_message(AssistantAnswer(answer, per_turn))
        except Exception as e:  # noqa: BLE001
            self.post_message(AgentError(str(e)))

    # ----- F5 / [run] direct pivot -----

    def action_run_pivot(self) -> None:
        if self.busy:
            self._chat_note("[yellow]busy[/yellow]")
            return
        rows = list(self.rows_order)
        cols = list(self.cols_order)
        measure = self.query_one("#sel-measure", Select).value
        where = (self.query_one("#where-input", Input).value or "").strip() or None
        if not rows:
            self._chat_note(
                "[red]pick at least one row dimension "
                "(F4 → highlight a dim → press `r` to add to rows)[/red]"
            )
            return
        self.busy = True
        rows_label = ",".join(rows)
        cols_label = ",".join(cols) if cols else "(none)"
        label = (
            f"pivot rows=[{rows_label}] cols=[{cols_label}] measure={measure}"
            + (f" where=({where})" if where else "")
        )
        self._chat_note(f"\n[bold green]you>[/bold green] [dim]({label})[/dim]")
        self._run_pivot_threaded(rows, cols, str(measure), where, label)

    @work(thread=True, exclusive=False)
    def _run_pivot_threaded(
        self,
        rows: list[str],
        cols: list[str],
        measure: str,
        where: Optional[str],
        label: str,
    ) -> None:
        try:
            r = execute_pivot_direct(
                self.state,
                self.con,
                rows=rows,
                cols=cols,
                measure=measure,
                where=where,
            )
            self.post_message(DirectPivotResult(label, r))
        except Exception as e:  # noqa: BLE001
            self.post_message(AgentError(str(e)))

    # ----- message handlers -----

    def on_agent_event_message(self, msg: AgentEventMessage) -> None:
        v = self.query_one("#verbose-log", RichLog)
        e = msg.event
        ts = datetime.now().strftime("%H:%M:%S")
        match e.kind:
            case "hop":
                v.write(
                    f"[bold cyan]{ts}[/bold cyan] [bold]hop {e.data['n']}/{e.data['total']}[/bold] "
                    f"[dim]{e.data['model']}[/dim]"
                )
            case "schema":
                v.write("  [yellow]↪ schema_lookup[/yellow]")
            case "sql":
                tool = e.data.get("tool", "?")
                if tool == "pivot":
                    v.write(
                        f"  [yellow]↪ pivot[/yellow] rows={e.data.get('rows')} "
                        f"cols={e.data.get('cols')} measure={e.data.get('measure')}"
                    )
                else:
                    preview = e.data.get("sql_preview", "")
                    v.write(f"  [yellow]↪ run_sql[/yellow] [italic]{preview!r}[/italic]")
            case "sql_result":
                err = e.data.get("error")
                if err:
                    v.write(f"  [red]× sql error: {err}[/red]")
                else:
                    v.write(
                        f"  [green]← {e.data['row_count']} rows[/green] "
                        f"[dim]({e.data['elapsed_ms']} ms)[/dim]"
                    )
            case "response":
                v.write(
                    f"  [dim]content={e.data['content_len']}ch  "
                    f"thinking={e.data['thinking_len']}ch  "
                    f"tools={e.data['tool_call_count']}[/dim]"
                )
            case "synthesis":
                v.write(
                    f"  [magenta]⚡ tool budget hit — synthesising over "
                    f"{e.data['query_count']} queries[/magenta]"
                )
            case "final":
                v.write(
                    f"  [bold green]✓ final[/bold green] [dim]len={e.data['length']}  "
                    f"queries={e.data['query_count']}[/dim]"
                )
            case "error":
                v.write(f"  [red]× {e.data}[/red]")
            case _:
                v.write(f"  [dim]{e.kind} {e.data}[/dim]")

    def on_assistant_answer(self, msg: AssistantAnswer) -> None:
        self.busy = False
        self.current_turn_queries = list(msg.queries)
        chat = self.query_one("#chat-log", RichLog)
        chat.write("\n[bold magenta]assistant>[/bold magenta]")
        chat.write(RichMarkdown(msg.text))

        # Sync the controls pane to whatever aggregation the LLM ran
        # most recently, so the human picks up where the LLM left off:
        # change one dim, F5, see the variant. We scan in reverse so
        # the latest match wins.
        #
        #   1. Explicit pivot tool call → exact sync.
        #   2. run_sql with a pivot-shaped column list (N dim cols +
        #      1 measure col, all names in our registries) → inferred
        #      sync. F5 reruns it via the pivot path (with rollup),
        #      which is a slight shape change but keeps the controls
        #      aligned with what produced the visible table.
        for q in reversed(msg.queries):
            if q.measure is not None:
                self._sync_controls_from_pivot(
                    q.row_dim_names, q.col_dim_names, q.measure, q.where
                )
                break
            inferred = _try_infer_pivot_from_result(q)
            if inferred is not None:
                rows, cols, measure = inferred
                self._sync_controls_from_pivot(rows, cols, measure, None)
                break

        # Inline trace: every query the agent ran during this turn,
        # numbered so a digit shortcut (focus chat with F3 → 1..9)
        # or `/show N` can pop it in the detail modal.
        if msg.queries:
            chat.write(Rule(title=f"{len(msg.queries)} queries this turn", style="dim"))
            for i, q in enumerate(msg.queries, start=1):
                header = (
                    f"[bold yellow][{i}][/bold yellow] "
                    f"[dim]{q.elapsed_ms:>5.0f} ms · "
                    f"{q.row_count} rows · "
                    f"cols={len(q.columns)}[/dim]"
                )
                if q.row_dim_names:
                    header += (
                        f"  [dim]pivot rows=[{','.join(q.row_dim_names)}][/dim]"
                    )
                chat.write(header)
                # SQL preview — full text, dimly-coloured so the
                # tables that follow stay the focal point.
                sql_lines = q.sql.split("\n") if q.sql else ["(no sql)"]
                # Collapse leading/trailing whitespace per line for
                # readability — the generator emits one-liners that
                # `cat`-like terminals would wrap awkwardly.
                for line in sql_lines[:8]:
                    chat.write(f"  [dim italic cyan]{line.rstrip()}[/dim italic cyan]")
                if len(sql_lines) > 8:
                    chat.write(f"  [dim]… (+{len(sql_lines) - 8} more SQL lines)[/dim]")
                # Result preview — compact (8 rows) here in the chat;
                # the full result is one keypress away.
                if q.error:
                    chat.write(f"  [red]× {q.error}[/red]")
                elif q.row_count == 0:
                    chat.write("  [dim](no rows)[/dim]")
                else:
                    chat.write(_render_result_table(q, max_rows=8, max_cols=10))
            digits = ", ".join(str(i) for i in range(1, min(10, len(msg.queries) + 1)))
            chat.write(
                f"[dim]F3+{{digit}} preview · alt+{{digit}} load into "
                f"controls (or `/show N` / `/use N` from input) · "
                f"available: {digits}[/dim]"
            )
        chat.write(Rule(style="dim"))

    def on_direct_pivot_result(self, msg: DirectPivotResult) -> None:
        self.busy = False
        chat = self.query_one("#chat-log", RichLog)
        chat.write("\n[bold magenta]assistant>[/bold magenta] [dim](direct pivot)[/dim]")
        if msg.result.error:
            chat.write(f"[red]error:[/red] {msg.result.error}")
        else:
            chat.write(_render_result_table(msg.result))
        chat.write(Rule(style="dim"))

    def on_agent_error(self, msg: AgentError) -> None:
        self.busy = False
        self.query_one("#chat-log", RichLog).write(
            f"\n[bold red]error:[/bold red] {msg.err}"
        )

    # ----- helpers -----

    def _chat_note(self, line: str) -> None:
        self.query_one("#chat-log", RichLog).write(line)

    def _handle_slash(self, line: str) -> None:
        parts = line.strip().split(maxsplit=1)
        cmd = parts[0].lower()
        arg = parts[1] if len(parts) > 1 else ""
        match cmd:
            case "/quit" | "/exit":
                self.exit()
            case "/reset":
                self.action_reset()
            case "/clear":
                self.action_clear_chat()
            case "/model":
                if arg:
                    self.state.chat_model = arg.strip()
                    self._chat_note(
                        f"[dim italic]chat model set to {self.state.chat_model}[/dim italic]"
                    )
                else:
                    self._chat_note(
                        f"[dim italic]current chat model: {self.state.chat_model}[/dim italic]"
                    )
            case "/provider":
                parts_p = arg.strip().split()
                if not parts_p:
                    self._chat_note(
                        f"[dim italic]current: provider=[bold]{self.current_provider}[/bold] · "
                        f"model=[bold]{self.state.chat_model}[/bold]\n"
                        f"usage: /provider {{ollama|openai|gemini}} [model]\n"
                        f"defaults: " + ", ".join(
                            f"{p}={m}" for p, m in PROVIDER_DEFAULT_MODELS.items()
                        )
                        + "\nswitching providers resets the conversation."
                        + "[/dim italic]"
                    )
                    return
                new_prov = parts_p[0].lower()
                new_model = parts_p[1] if len(parts_p) > 1 else None
                try:
                    new_client = make_chat_client(new_prov)
                except Exception as e:  # noqa: BLE001
                    self._chat_note(f"[red]/provider: {e}[/red]")
                    return
                self.oc = new_client
                self.current_provider = new_prov
                self.state.chat_model = (
                    new_model
                    or PROVIDER_DEFAULT_MODELS.get(new_prov, self.state.chat_model)
                )
                # Conversation reset — tool_call_ids from the previous
                # provider would confuse the new one's strict validators.
                self.action_reset()
                self._chat_note(
                    f"[dim cyan]→ provider=[bold]{new_prov}[/bold]  "
                    f"model=[bold]{self.state.chat_model}[/bold]  "
                    f"(conversation reset)[/dim cyan]"
                )
            case "/sql":
                if not arg.strip():
                    self._chat_note("[red]usage: /sql SELECT ... FROM lake.nyc.yellow[/red]")
                    return
                self._chat_note(
                    f"\n[bold green]you>[/bold green] [dim](raw sql)[/dim] {arg}"
                )
                self.busy = True
                self._run_raw_sql_threaded(arg)
            case "/show":
                if not arg.strip().isdigit():
                    self._chat_note(
                        f"[red]usage: /show N  (N = 1..{len(self.current_turn_queries) or 0})[/red]"
                    )
                    return
                idx = int(arg.strip()) - 1
                if 0 <= idx < len(self.current_turn_queries):
                    self.push_screen(QueryDetail(self.current_turn_queries[idx]))
                else:
                    self._chat_note(
                        f"[red]no query #{idx + 1} — this turn ran "
                        f"{len(self.current_turn_queries)} queries[/red]"
                    )
            case "/use":
                if not arg.strip().isdigit():
                    self._chat_note(
                        f"[red]usage: /use N  (N = 1..{len(self.current_turn_queries) or 0}) "
                        f"— loads query N's pivot args into the controls so F5 reruns a variant[/red]"
                    )
                    return
                idx = int(arg.strip()) - 1
                self._load_query_into_controls(idx)
            case "/schema":
                from tables.agent import tool_schema_lookup
                try:
                    s = tool_schema_lookup(self.con)
                    self._chat_note(
                        f"\n[bold]View:[/bold] {s['view_to_query']}  "
                        f"[bold]Columns:[/bold] {len(s['view_columns'])} · "
                        f"[bold]Dims:[/bold] {len(s['dimension_names'])} · "
                        f"[bold]Measures:[/bold] {len(s['measure_names'])}"
                    )
                    for name, type_ in s["view_columns"].items():
                        self._chat_note(f"  • {name}  [dim]{type_}[/dim]")
                except Exception as e:  # noqa: BLE001
                    self._chat_note(f"[red]schema: {e}[/red]")
            case "/help":
                self._chat_note(
                    "[dim]"
                    "  /reset           clear conversation\n"
                    "  /clear           clear the chat pane (history kept)\n"
                    "  /sql SELECT…     raw SQL escape hatch\n"
                    "  /schema          dump column list\n"
                    "  /model X         switch chat model (must support tools)\n"
                    "  /provider P [M]  switch provider (ollama|openai|gemini), "
                    "optionally also model — resets conversation\n"
                    "  /show N          open query N's full result in modal\n"
                    "  /use N           load query N's pivot args into controls\n"
                    "  /quit            exit\n"
                    "Keys: ↑/↓ history · F2/F3/F4 focus input/chat/controls · "
                    "F5 run pivot · F6-F9 resize · 1-9 preview · "
                    "alt+1..9 load query into controls · "
                    "ctrl+r reset · ctrl+l clear · ctrl+c quit"
                    "[/dim]"
                )
            case _:
                self._chat_note(f"[red]unknown command {cmd} — /help for list[/red]")

    @work(thread=True, exclusive=False)
    def _run_raw_sql_threaded(self, sql: str) -> None:
        try:
            r = tool_run_sql(self.con, sql=sql)
            self.state.last_queries.append(r)
            self.post_message(DirectPivotResult("raw sql", r))
        except Exception as e:  # noqa: BLE001
            self.post_message(AgentError(str(e)))

    # ----- key routing -----
    #
    # Three modes:
    #   1) `#question` input focused → ↑/↓ cycle input history
    #   2) `#dim-list` / `#rows-list` / `#cols-list` focused → vim nav
    #      + r/c add + J/K reorder + d/del remove
    #   3) `#verbose-log` focused → `p` opens latest-query preview

    async def on_key(self, event) -> None:  # type: ignore[override]
        # ----- input history (mode 1) -----
        try:
            inp = self.query_one("#question", Input)
        except Exception:  # noqa: BLE001
            inp = None
        if inp is not None and self.focused is inp:
            if event.key == "up":
                if not self.history:
                    return
                if self.history_idx == len(self.history):
                    self.input_draft = inp.value
                if self.history_idx > 0:
                    self.history_idx -= 1
                    inp.value = self.history[self.history_idx]
                    inp.cursor_position = len(inp.value)
                    event.prevent_default()
                    event.stop()
                return
            if event.key == "down":
                if not self.history:
                    return
                if self.history_idx < len(self.history) - 1:
                    self.history_idx += 1
                    inp.value = self.history[self.history_idx]
                    inp.cursor_position = len(inp.value)
                else:
                    self.history_idx = len(self.history)
                    inp.value = self.input_draft
                    inp.cursor_position = len(inp.value)
                event.prevent_default()
                event.stop()
                return

        # ----- pivot-controls lists (mode 2) -----
        focus = self.focused
        focus_id = focus.id if focus else None
        if focus_id in ("dim-list", "rows-list", "cols-list") and isinstance(focus, ListView):
            handled = self._handle_controls_key(focus, focus_id, event)
            if handled:
                event.prevent_default()
                event.stop()
                return

        # ----- preview (mode 3) -----
        if event.key == "p":
            try:
                verbose = self.query_one("#verbose-log", RichLog)
            except Exception:  # noqa: BLE001
                verbose = None
            if verbose is not None and self.focused is verbose and self.state.last_queries:
                self.push_screen(QueryDetail(self.state.last_queries[-1]))
                event.prevent_default()
                event.stop()
                return

        # ----- digit shortcut on focused chat → drill into a per-turn query -----
        if event.key in {"1", "2", "3", "4", "5", "6", "7", "8", "9"}:
            try:
                chat = self.query_one("#chat-log", RichLog)
            except Exception:  # noqa: BLE001
                chat = None
            if chat is not None and self.focused is chat:
                idx = int(event.key) - 1
                if 0 <= idx < len(self.current_turn_queries):
                    self.push_screen(QueryDetail(self.current_turn_queries[idx]))
                    event.prevent_default()
                    event.stop()
                    return

        # ----- alt+1..alt+9: load query N's pivot args into the controls -----
        # Works from any focus (you don't need to focus chat first), so
        # the human can "follow up on query [N]" without breaking flow.
        if event.key in {f"alt+{d}" for d in "123456789"}:
            idx = int(event.key.split("+")[1]) - 1
            self._load_query_into_controls(idx)
            event.prevent_default()
            event.stop()

    def _handle_controls_key(self, lv: ListView, lv_id: str, event) -> bool:
        """Returns True if the key was consumed."""
        key = event.key
        # Vim navigation — map j/k to ↓/↑ regardless of which list is focused.
        if key == "j":
            lv.action_cursor_down()
            return True
        if key == "k":
            lv.action_cursor_up()
            return True

        # All-dims list: r → rows, c → cols
        if lv_id == "dim-list":
            if key in ("r", "c"):
                if lv.index is None or lv.index < 0:
                    return True
                item = lv.children[lv.index]
                name = getattr(item, "name", None)
                if not name:
                    return True
                if key == "r":
                    self._add_to_rows(name)
                else:
                    self._add_to_cols(name)
                return True
            return False

        # rows-list / cols-list: d/del remove, J/K reorder
        which = "rows" if lv_id == "rows-list" else "cols"
        if key in ("d", "delete"):
            if lv.index is not None and lv.index >= 0:
                self._remove_from(which, lv.index)
            return True
        if key == "J":  # shift+j → move down
            if lv.index is not None:
                self._reorder_within(which, lv.index, +1)
            return True
        if key == "K":  # shift+k → move up
            if lv.index is not None:
                self._reorder_within(which, lv.index, -1)
            return True
        return False


def main() -> int:
    TablesChat().run()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
