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
    RichLog,
    Select,
    SelectionList,
    Static,
)
from textual.widgets.selection_list import Selection

from shared.tui_widgets import Splitter
from shared.pivot_sql import DIMENSIONS, MEASURES, build_pivot_sql

from tables.agent import (
    AgentEvent,
    BUCKET,
    CHAT_MODEL,
    DEFAULT_ROW_LIMIT,
    NAMESPACE,
    OLLAMA_HOST,
    QueryResult,
    TABLE,
    TablesState,
    execute_pivot_direct,
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


def _render_result_table(r: QueryResult, *, max_rows: int = 200) -> RichTable:
    """Render a QueryResult.

    If `row_dim_names` is set (pivot path), fold the N row-dim columns
    into a single indented "hierarchy" column with TOTAL row at top,
    subtotals bold, leaves plain. The `_g_*` GROUPING flags drive the
    level detection; they're never displayed.

    If `row_dim_names` is empty (raw `/sql` or schema lookup), fall
    back to a flat one-column-per-result-column render.
    """
    title = (
        f"{r.row_count} row{'s' if r.row_count != 1 else ''} "
        f"({r.elapsed_ms:.0f} ms)"
    )

    # ── Flat path: no hierarchy info ──
    if not r.row_dim_names:
        t = RichTable(title=title, show_header=True, header_style="bold cyan")
        for c in r.columns:
            t.add_column(str(c), justify="right")
        for row in r.rows[:max_rows]:
            t.add_row(*[_fmt_value(v) for v in row])
        if r.row_count > max_rows:
            t.caption = f"showing first {max_rows} of {r.row_count}"
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
        )
    hidden_for_display = set(g_idx)  # never show GROUPING flags
    measure_idx = [
        i for i in range(len(r.columns))
        if i not in hidden_for_display and i not in dim_idx
    ]
    measure_names = [r.columns[i] for i in measure_idx]

    t = RichTable(title=title, show_header=True, header_style="bold cyan")
    # One column per row dim — clear, unambiguous.
    for name in r.row_dim_names:
        t.add_column(name, no_wrap=True)
    for name in measure_names:
        t.add_column(name, justify="right")

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
        t.add_row(*dim_cells, *measure_cells, style=style)

    if r.row_count > max_rows:
        t.caption = f"showing first {max_rows} of {r.row_count}"
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
    #controls-form Select, #controls-form SelectionList {
        margin-bottom: 0;
    }
    #controls-form SelectionList {
        /* Cap at ~6 visible rows so the controls pane isn't dominated
           by a huge list. SelectionList scrolls internally. */
        height: 7;
        border: round $primary;
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
        self.oc = make_ollama_client()
        self.con = make_duckdb_connection()
        self.history: list[str] = []
        self.history_idx: int = 0
        self.input_draft: str = ""

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
                        "pivot controls  (F4 focus · space toggle · F5 run)",
                        classes="pane-title",
                    )
                    meas_opts = [(m.name, m.name) for m in MEASURES.values()]
                    # SelectionList[T] takes Selection(prompt, value,
                    # initial_state). Default rows = hour_of_day,
                    # default cols = nothing checked.
                    rows_selections = [
                        Selection(d.name, d.name, d.name == "hour_of_day")
                        for d in DIMENSIONS.values()
                    ]
                    cols_selections = [
                        Selection(d.name, d.name, False)
                        for d in DIMENSIONS.values()
                    ]
                    with Vertical(id="controls-form"):
                        yield Static("rows (1+):", classes="hint")
                        yield SelectionList[str](
                            *rows_selections, id="sel-rows"
                        )
                        yield Static("cols (0+, cross-product spread):", classes="hint")
                        yield SelectionList[str](
                            *cols_selections, id="sel-cols"
                        )
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

        self.query_one("#question", Input).focus()

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
        self.query_one("#sel-rows", SelectionList).focus()

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

        try:
            answer = run_turn(
                self.state, self.oc, self.con, question, on_event=emit
            )
            self.post_message(
                AssistantAnswer(answer, list(self.state.last_queries))
            )
        except Exception as e:  # noqa: BLE001
            self.post_message(AgentError(str(e)))

    # ----- F5 / [run] direct pivot -----

    def action_run_pivot(self) -> None:
        if self.busy:
            self._chat_note("[yellow]busy[/yellow]")
            return
        rows = list(self.query_one("#sel-rows", SelectionList).selected)
        cols = list(self.query_one("#sel-cols", SelectionList).selected)
        measure = self.query_one("#sel-measure", Select).value
        where = (self.query_one("#where-input", Input).value or "").strip() or None
        if not rows:
            self._chat_note(
                "[red]pick at least one row dimension (space to toggle in the rows list)[/red]"
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
        chat = self.query_one("#chat-log", RichLog)
        chat.write("\n[bold magenta]assistant>[/bold magenta]")
        chat.write(RichMarkdown(msg.text))
        chat.write(f"[dim]  ({len(msg.queries)} queries)[/dim]")
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
            case "/sql":
                if not arg.strip():
                    self._chat_note("[red]usage: /sql SELECT ... FROM lake.nyc.yellow[/red]")
                    return
                self._chat_note(
                    f"\n[bold green]you>[/bold green] [dim](raw sql)[/dim] {arg}"
                )
                self.busy = True
                self._run_raw_sql_threaded(arg)
            case "/schema":
                from tables.agent import tool_schema_lookup
                try:
                    s = tool_schema_lookup(self.con)
                    self._chat_note(
                        f"\n[bold]Columns:[/bold] {len(s['columns'])} · "
                        f"[bold]Dims:[/bold] {len(s['dimensions'])} · "
                        f"[bold]Measures:[/bold] {len(s['measures'])}"
                    )
                    for c in s["columns"]:
                        self._chat_note(f"  • {c['name']}  [dim]{c['type']}[/dim]")
                except Exception as e:  # noqa: BLE001
                    self._chat_note(f"[red]schema: {e}[/red]")
            case "/help":
                self._chat_note(
                    "[dim]"
                    "  /reset        clear conversation\n"
                    "  /clear        clear the chat pane (history kept)\n"
                    "  /sql SELECT…  raw SQL escape hatch\n"
                    "  /schema       dump column list\n"
                    "  /model X      switch chat model (must support tools)\n"
                    "  /quit         exit\n"
                    "Keys: ↑/↓ history · F2/F3/F4 focus input/chat/controls · "
                    "F5 run pivot · F6-F9 resize · p preview · ctrl+r reset · "
                    "ctrl+l clear · ctrl+c quit"
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

    # ----- key routing: input-history nav, then `p` preview -----

    async def on_key(self, event) -> None:  # type: ignore[override]
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
        # 'p' on verbose-log focus opens the most-recent query's detail
        if event.key == "p":
            verbose = self.query_one("#verbose-log", RichLog)
            if self.focused is verbose and self.state.last_queries:
                self.push_screen(QueryDetail(self.state.last_queries[-1]))
                event.prevent_default()
                event.stop()


def main() -> int:
    TablesChat().run()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
