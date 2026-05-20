"""Textual TUI for agentic RAG over the parlis-indexed marila vectors.

Layout (terminals ≥ 100 cols):

    ┌───────────────────────────────┬───────────────────────────────┐
    │  chat                         │  hops (verbose)               │
    │  (you / assistant turns)      │                               │
    │                               │  [hop 1/50] gemma4 …          │
    │  you> Welche Anfragen …       │   ↪ search "Windenergie", k=5 │
    │                               │   ← content=480ch, …          │
    │  assistant>                   │                               │
    │  Die Drucksachen behandeln …  ├───────────────────────────────┤
    │                               │  sources                      │
    │                               │  ▶ 0.508  17_1341_D.pdf       │
    │                               │    0.523  17_10074_D.pdf      │
    │                               │    …                          │
    ├───────────────────────────────┴───────────────────────────────┤
    │ > ask a question…                                             │
    │ ↑/↓ history · p preview · ctrl+r reset · ctrl+l clear · ^c    │
    └───────────────────────────────────────────────────────────────┘

Run:
    cd demo && uv run ./parlis_chat.py

Env knobs (see `parlis_agent.py` for the full list):
    BUCKET=parlis  INDEX=drucksachen  CHAT_MODEL=gemma4:latest
    EMBED_MODEL=embeddinggemma:latest  MARILA_ENDPOINT=http://localhost:8080
"""

from __future__ import annotations

from datetime import datetime
from typing import Any

from rich.markdown import Markdown as RichMarkdown
from rich.rule import Rule
from rich.text import Text

from textual import work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Container, Horizontal, Vertical
from textual.message import Message
from textual.reactive import reactive
from textual.screen import ModalScreen
from textual.widgets import (
    Button,
    Footer,
    Header,
    Input,
    Label,
    ListItem,
    ListView,
    Markdown,
    RichLog,
    Static,
)

from parlis_agent import (
    AgentEvent,
    BUCKET,
    CHAT_MODEL,
    ChatState,
    EMBED_MODEL,
    INDEX,
    MARILA_ENDPOINT,
    OLLAMA_HOST,
    ChatState,
    fetch_full_chunk,
    make_ollama_client,
    make_vectors_client,
    preflight_index,
    run_turn,
)


# ---------------------------------------------------------------------------
# Custom messages — let the @work thread push state back to the main loop
# ---------------------------------------------------------------------------


class AgentEventMessage(Message):
    """An `AgentEvent` produced by the agent thread, marshalled across
    to the Textual event loop so widgets can update safely."""

    def __init__(self, event: AgentEvent) -> None:
        super().__init__()
        self.event = event


class AssistantAnswer(Message):
    """Final answer string from a completed `run_turn`."""

    def __init__(self, text: str, sources: list[dict[str, Any]]) -> None:
        super().__init__()
        self.text = text
        self.sources = sources


class AgentError(Message):
    def __init__(self, err: str) -> None:
        super().__init__()
        self.err = err


# ---------------------------------------------------------------------------
# Source-preview modal
# ---------------------------------------------------------------------------


class SourcePreview(ModalScreen[None]):
    """Modal popup showing a chunk's full text + metadata. Esc to close."""

    BINDINGS = [
        Binding("escape", "dismiss(None)", "Close"),
        Binding("q", "dismiss(None)", "Close"),
    ]

    DEFAULT_CSS = """
    SourcePreview {
        align: center middle;
    }
    #preview-box {
        width: 80%;
        height: 80%;
        background: $surface;
        border: thick $accent;
        padding: 1 2;
    }
    #preview-title {
        text-style: bold;
        color: $accent;
    }
    #preview-meta {
        color: $text-muted;
        margin-bottom: 1;
    }
    #preview-body {
        background: $background;
        padding: 1;
        height: 1fr;
        border: round $primary;
    }
    """

    def __init__(self, source: dict[str, Any]) -> None:
        super().__init__()
        self.source = source

    def compose(self) -> ComposeResult:
        s = self.source
        dist = s.get("distance")
        dist_s = f"  cos-dist {dist:.4f}" if isinstance(dist, (int, float)) else ""
        chunk = s.get("chunk_idx")
        chunk_s = f"  chunk_idx {chunk}" if chunk is not None else ""
        with Container(id="preview-box"):
            yield Label(s.get("source", "?"), id="preview-title")
            yield Label(f"{dist_s}{chunk_s}", id="preview-meta")
            body = s.get("snippet") or "(no snippet in metadata)"
            log = RichLog(id="preview-body", wrap=True, markup=False, highlight=False)
            # Snippets are raw PDF-extracted plain text, not markdown —
            # render as plain Text so we don't try to interpret stray
            # asterisks or backticks as formatting.
            log.write(Text(body))
            yield log
            yield Label(
                "Esc / q to close",
                classes="hint",
            )


# ---------------------------------------------------------------------------
# Main app
# ---------------------------------------------------------------------------


class ParlisChat(App[None]):
    # Pane sizing — percentages so we can resize at runtime via F-keys
    # without re-running compose(). The reactive setters in
    # `_apply_layout` write `styles.width / height` back onto the
    # mounted widgets.
    CHAT_WIDTH_MIN, CHAT_WIDTH_MAX = 30, 85
    VERBOSE_HEIGHT_MIN, VERBOSE_HEIGHT_MAX = 15, 85

    CSS = """
    Screen {
        layers: base modal;
    }
    #main {
        layout: horizontal;
        height: 1fr;
    }
    #chat-pane {
        width: 66%;
        border: round $primary;
    }
    #side-pane {
        width: 34%;
        layout: vertical;
    }
    #verbose-pane {
        height: 50%;
        border: round $secondary;
    }
    #sources-pane {
        height: 50%;
        border: round $secondary;
    }
    #sources-list {
        height: 1fr;
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
        # `ctrl+a` is Emacs-style "beginning-of-line" inside Input and
        # gets consumed there, so we use F2 + F3 + F4 for pane focus
        # which no widget binds by default.
        Binding("f2", "focus_input", "→ input"),
        Binding("f3", "focus_chat", "→ chat (scroll)"),
        Binding("f4", "focus_sources", "→ sources"),
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
        self.state = ChatState()
        self.state.reset()
        self.oc = make_ollama_client()
        self.vc = make_vectors_client()
        self.history: list[str] = []      # user-input history for up/down
        self.history_idx: int = 0         # current navigation position
        self.input_draft: str = ""        # text typed before they hit up

    # ----- layout -----

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        with Horizontal(id="main"):
            with Vertical(id="chat-pane"):
                yield Static("chat  (F3 focus · then ↑↓ / PgUp PgDn / wheel scroll)", classes="pane-title")
                yield RichLog(
                    id="chat-log",
                    wrap=True,
                    markup=True,
                    highlight=False,
                    auto_scroll=True,
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
                with Vertical(id="sources-pane"):
                    yield Static(
                        "sources  (F4 focus · ↑↓ select · p preview)",
                        classes="pane-title",
                    )
                    yield ListView(id="sources-list")
        yield Input(
            placeholder="ask (↑↓ history · F2/F3/F4 focus · F6-F9 resize · ctrl+r reset · ctrl+c quit)",
            id="question",
        )
        yield Footer()

    # ----- lifecycle -----

    def on_mount(self) -> None:
        self.title = "marila — agentic RAG chat"
        self.sub_title = f"{BUCKET}/{INDEX}"
        chat = self.query_one("#chat-log", RichLog)
        chat.write(
            f"[dim]embed: {EMBED_MODEL}   chat: {self.state.chat_model}   "
            f"ollama: {OLLAMA_HOST}[/dim]"
        )
        verbose = self.query_one("#verbose-log", RichLog)
        verbose.write("[dim]waiting for first question…[/dim]")

        # Preflight the index — fail fast with a banner instead of bad
        # vibes when the user types their first question.
        ok, msg = preflight_index(self.vc)
        if ok:
            chat.write(f"[green]✓ {msg}[/green]")
        else:
            chat.write(
                f"[red]✗ could not reach {BUCKET}/{INDEX}: {msg}[/red]\n"
                f"[dim]Run `demo/index_parlis.sh` first or set BUCKET/INDEX.[/dim]"
            )

        self.query_one("#question", Input).focus()

    # ----- key bindings -----

    def action_reset(self) -> None:
        self.state.reset()
        self.query_one("#chat-log", RichLog).write(
            "\n[dim italic]— conversation reset —[/dim italic]\n"
        )
        self.query_one("#verbose-log", RichLog).clear()
        self.query_one("#sources-list", ListView).clear()
        self.state.last_sources = []
        self.history_idx = len(self.history)

    def action_clear_chat(self) -> None:
        self.query_one("#chat-log", RichLog).clear()

    def action_focus_sources(self) -> None:
        self.query_one("#sources-list", ListView).focus()

    def action_focus_input(self) -> None:
        self.query_one("#question", Input).focus()

    def action_focus_chat(self) -> None:
        """Focus the chat log so PgUp / PgDn / arrows / mouse wheel scroll it."""
        chat = self.query_one("#chat-log", RichLog)
        chat.can_focus = True   # RichLog isn't focus-by-default in older textual
        chat.focus()

    # ----- pane resize -----

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
        """Write current reactive sizes onto the mounted panes."""
        try:
            chat_pane = self.query_one("#chat-pane")
            side_pane = self.query_one("#side-pane")
            verbose_pane = self.query_one("#verbose-pane")
            sources_pane = self.query_one("#sources-pane")
        except Exception:  # noqa: BLE001 — pre-mount call
            return
        chat_pane.styles.width = f"{self.chat_width_pct}%"
        side_pane.styles.width = f"{100 - self.chat_width_pct}%"
        verbose_pane.styles.height = f"{self.verbose_height_pct}%"
        sources_pane.styles.height = f"{100 - self.verbose_height_pct}%"

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

        # Push into history (no dup of the previous one).
        if not self.history or self.history[-1] != text:
            self.history.append(text)
        self.history_idx = len(self.history)

        # Slash commands
        if text.startswith("/"):
            self._handle_slash(text)
            return

        self._chat_note(f"\n[bold green]you>[/bold green] {text}")
        self.busy = True
        self.handle_turn(text)

    # ----- sources list -----

    def on_list_view_selected(self, event: ListView.Selected) -> None:
        # Selection alone doesn't trigger preview — the user has to press p.
        # (Avoids opening a modal on every arrow key.)
        pass

    def _handle_list_p(self) -> None:
        """Preview the highlighted source."""
        lv = self.query_one("#sources-list", ListView)
        idx = lv.index
        if idx is None or idx < 0 or idx >= len(self.state.last_sources):
            return
        src = dict(self.state.last_sources[idx])  # copy
        # Try to enrich via GetVectors so we get the full untruncated snippet.
        key = src.get("key")
        if key:
            try:
                richer = fetch_full_chunk(self.vc, key)
                if richer and not richer.get("error"):
                    # Keep distance from the search hit; everything else from the
                    # full fetch is canonical.
                    richer["distance"] = src.get("distance")
                    src = richer
            except Exception:  # noqa: BLE001
                pass
        self.push_screen(SourcePreview(src))

    def _maybe_handle_sources_p(self, key: str) -> bool:
        sources_list = self.query_one("#sources-list", ListView)
        if self.focused is sources_list and key == "p":
            self._handle_list_p()
            return True
        return False

    # ----- agent worker -----

    @work(thread=True, exclusive=True)
    def handle_turn(self, question: str) -> None:
        """Run the agent on a background thread; marshal events back via
        post_message so widgets are touched only on the main loop."""

        def emit(ev: AgentEvent) -> None:
            self.post_message(AgentEventMessage(ev))

        try:
            answer = run_turn(self.state, self.oc, self.vc, question, on_event=emit)
            self.post_message(AssistantAnswer(answer, list(self.state.last_sources)))
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
            case "search":
                q = e.data["query"]
                k = e.data["k"]
                v.write(f"  [yellow]↪ search[/yellow] [italic]{q!r}[/italic] k={k}")
            case "results":
                err = e.data.get("error")
                if err:
                    v.write(f"  [red]× search error: {err}[/red]")
                else:
                    n = e.data["hit_count"]
                    td = e.data.get("top_distance")
                    td_s = f" top d={td:.4f}" if isinstance(td, (int, float)) else ""
                    v.write(f"  [green]← {n} hits[/green]{td_s}")
            case "response":
                v.write(
                    f"  [dim]content={e.data['content_len']}ch  "
                    f"thinking={e.data['thinking_len']}ch  "
                    f"tools={e.data['tool_call_count']}[/dim]"
                )
            case "synthesis":
                v.write(
                    f"  [magenta]⚡ tool budget hit — synthesising over "
                    f"{e.data['source_count']} sources[/magenta]"
                )
            case "final":
                v.write(
                    f"  [bold green]✓ final[/bold green] [dim]len={e.data['length']}  "
                    f"sources={e.data['source_count']}[/dim]"
                )
            case "error":
                v.write(f"  [red]× {e.data}[/red]")
            case _:
                v.write(f"  [dim]{e.kind} {e.data}[/dim]")

    def on_assistant_answer(self, msg: AssistantAnswer) -> None:
        self.busy = False
        chat = self.query_one("#chat-log", RichLog)
        chat.write("\n[bold magenta]assistant>[/bold magenta]")
        # RichLog accepts any Rich renderable. Wrap the answer as
        # Markdown so headings / lists / bold / code spans / tables /
        # fenced code blocks render properly — the system prompt tells
        # the model to emit GFM.
        chat.write(RichMarkdown(msg.text))
        chat.write(f"[dim]  ({len(msg.sources)} sources)[/dim]")
        chat.write(Rule(style="dim"))

        # Repopulate the sources list.
        lv = self.query_one("#sources-list", ListView)
        lv.clear()
        for s in msg.sources:
            dist = s.get("distance")
            dist_s = f"{dist:.4f}" if isinstance(dist, (int, float)) else "  ?  "
            src = s.get("source", "?")
            # Trim long paths so the right-pane stays readable
            short = src.rsplit("/", 1)[-1] if "/" in src else src
            lv.append(ListItem(Label(f"{dist_s}  {short}")))

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
            case "/k":
                try:
                    self.state.default_k = max(1, int(arg))
                    self._chat_note(
                        f"[dim italic]default k = {self.state.default_k}[/dim italic]"
                    )
                except ValueError:
                    self._chat_note("[red]usage: /k <int>[/red]")
            case "/help":
                self._chat_note(
                    "[dim]"
                    "  /reset        clear conversation\n"
                    "  /clear        clear the chat pane (history kept)\n"
                    "  /model X      switch chat model (must support tools)\n"
                    "  /k N          change default top-K\n"
                    "  /quit         exit\n"
                    "Keys: ↑/↓ history · ctrl+s focus sources · p preview · "
                    "ctrl+r reset · ctrl+l clear · ctrl+c quit"
                    "[/dim]"
                )
            case _:
                self._chat_note(f"[red]unknown command {cmd} — /help for list[/red]")

    # Catch 'p' globally so it works whenever the sources list is focused.
    async def on_key(self, event) -> None:  # type: ignore[override]
        # First: input-history nav.
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
        # Then: 'p' preview when sources list is focused.
        if event.key == "p":
            if self._maybe_handle_sources_p("p"):
                event.prevent_default()
                event.stop()


def main() -> int:
    ParlisChat().run()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
