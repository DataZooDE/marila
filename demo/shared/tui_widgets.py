"""Reusable Textual widgets shared by the vector/ and tables/ TUIs.

Lifted from `vector/chat.py` (originally `parlis_chat.py`). No
behavioural changes — the Splitter is exactly the one that was
inline there. Both TUIs now `from shared.tui_widgets import Splitter`.
"""

from __future__ import annotations

from typing import Literal

from rich.text import Text

from textual import events
from textual.widget import Widget


class Splitter(Widget):
    """A thin draggable bar between two sibling panes.

    `orientation='vertical'` puts a 1-col-wide bar that resizes the
    sibling to its left along the X axis. `orientation='horizontal'`
    puts a 1-row-tall bar that resizes the sibling above it along the
    Y axis.

    The sibling identified by `target_id` carries the percentage size;
    the other sibling is expected to be `1fr` so the remaining space
    flows to it automatically. The percentage is also mirrored to a
    reactive attribute on the App via `app_attr` so F-key bindings on
    the App and the drag handler here stay in sync.

    Callers (App subclasses) must define a `_apply_layout()` method
    that reads its reactive sizing attrs and writes the matching
    `styles.width` / `styles.height` onto the panes. The drag handler
    invokes it after each move; F-key actions do the same.
    """

    DEFAULT_CSS = """
    Splitter {
        background: $primary 40%;
    }
    Splitter:hover {
        background: $accent 60%;
    }
    Splitter.dragging {
        background: $accent;
    }
    Splitter.-vertical {
        width: 1;
        height: 1fr;
    }
    Splitter.-horizontal {
        width: 1fr;
        height: 1;
    }
    """

    def __init__(
        self,
        *,
        orientation: Literal["vertical", "horizontal"],
        target_id: str,
        app_attr: str,
        min_pct: int,
        max_pct: int,
        widget_id: str | None = None,
    ) -> None:
        super().__init__(id=widget_id)
        self.orientation = orientation
        self.target_id = target_id
        self.app_attr = app_attr
        self.min_pct = min_pct
        self.max_pct = max_pct
        self.add_class(f"-{orientation}")
        self._drag_start_screen: int = 0
        self._drag_start_target: int = 0
        self._dragging: bool = False

    def on_mouse_down(self, event: events.MouseDown) -> None:
        target = self.app.query_one(f"#{self.target_id}")
        if self.orientation == "vertical":
            self._drag_start_screen = event.screen_x
            self._drag_start_target = target.size.width
        else:
            self._drag_start_screen = event.screen_y
            self._drag_start_target = target.size.height
        self._dragging = True
        self.add_class("dragging")
        self.capture_mouse()

    def on_mouse_move(self, event: events.MouseMove) -> None:
        if not self._dragging:
            return
        parent = self.parent
        if parent is None:
            return
        if self.orientation == "vertical":
            delta = event.screen_x - self._drag_start_screen
            new_size = self._drag_start_target + delta
            container = parent.size.width
        else:
            delta = event.screen_y - self._drag_start_screen
            new_size = self._drag_start_target + delta
            container = parent.size.height
        if container <= 0:
            return
        new_pct = round(new_size * 100 / container)
        new_pct = max(self.min_pct, min(self.max_pct, new_pct))
        setattr(self.app, self.app_attr, new_pct)
        # Only the App knows how to rewrite the styles + keep both
        # axes in sync — defer to it.
        if hasattr(self.app, "_apply_layout"):
            self.app._apply_layout()  # type: ignore[attr-defined]

    def on_mouse_up(self, event: events.MouseUp) -> None:
        self._dragging = False
        self.remove_class("dragging")
        self.release_mouse()

    def render(self) -> Text:
        # Without an explicit render(), Widget falls back to repr-style
        # output (`Splitter#vsplit.-vertical`) which then literally
        # shows up inside the splitter. Render empty content; the
        # background colour from CSS still paints.
        return Text("")
