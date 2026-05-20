"""Agent-loop core shared by the TUI (`parlis_chat.py`) and the
old-style REPL (kept around as a smoke target). Decoupled so the TUI
can subscribe to per-hop events via a callback without owning any
display concerns.

Talks to local Ollama for both embeddings (default
`embeddinggemma:latest`) and chat (default `gemma4:latest` —
overridable via the `CHAT_MODEL` env var).
"""

from __future__ import annotations

import json
import os
import textwrap
from dataclasses import dataclass, field
from typing import Any, Callable, Optional

import boto3
import ollama

# ---------------------------------------------------------------------------
# Config (env-overridable)
# ---------------------------------------------------------------------------

MARILA_ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
MARILA_REGION = os.environ.get("MARILA_REGION", "eu-west-1")
MARILA_ACCESS_KEY = os.environ.get("MARILA_ACCESS_KEY_ID", "marila")
MARILA_SECRET = os.environ.get("MARILA_SECRET_ACCESS_KEY", "marilasecret")

BUCKET = os.environ.get("BUCKET", "parlis")
INDEX = os.environ.get("INDEX", "drucksachen")

OLLAMA_HOST = os.environ.get("OLLAMA_ENDPOINT", "http://localhost:11434")
EMBED_MODEL = os.environ.get("EMBED_MODEL", "embeddinggemma:latest")
CHAT_MODEL = os.environ.get("CHAT_MODEL", "gemma4:latest")

MAX_TOOL_HOPS = int(os.environ.get("MAX_TOOL_HOPS", "50"))
DEFAULT_K = int(os.environ.get("DEFAULT_K", "5"))

SYSTEM_PROMPT = textwrap.dedent(
    """\
    You are a research assistant for German parliamentary documents
    (Drucksachen) indexed in a vector store. Answer the user's question
    by calling the `search_parlis` tool with focused, well-phrased
    queries.

    Rules:
      - Always search before answering substantive questions. Don't
        rely on training-data recall for facts about specific
        Drucksachen.
      - Use AT MOST 3 searches per question. After that, commit to an
        answer based on what you have — even if the evidence is partial
        or weak, you must summarise findings instead of searching again.
      - If the first search returns hits with cosine distance > 0.6,
        the corpus probably doesn't have a clean answer; say so and
        stop searching.
      - Cite every factual claim by source path, e.g. `[WP17/01234.pdf]`.
      - When a question is in German, answer in German; otherwise mirror
        the user's language.
      - If the search returned nothing useful, say so plainly — don't
        invent citations. A short honest answer beats a long invented one.
    """
).strip()

TOOL_DEFINITIONS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "search_parlis",
            "description": (
                "Semantic search over the parliamentary-document corpus. "
                "Embeds the query and returns the top-K most similar chunks "
                "with source path, snippet, and distance."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": (
                            "The search query. Phrase it the way the answer "
                            "would appear in a document (declarative, "
                            "specific). German queries work best for German "
                            "documents."
                        ),
                    },
                    "k": {
                        "type": "integer",
                        "description": (
                            f"Top-K hits to return (default {DEFAULT_K}, "
                            "max 20)."
                        ),
                        "minimum": 1,
                        "maximum": 20,
                    },
                },
                "required": ["query"],
            },
        },
    },
]

# ---------------------------------------------------------------------------
# Clients
# ---------------------------------------------------------------------------


def make_vectors_client():
    return boto3.client(
        "s3vectors",
        endpoint_url=MARILA_ENDPOINT,
        region_name=MARILA_REGION,
        aws_access_key_id=MARILA_ACCESS_KEY,
        aws_secret_access_key=MARILA_SECRET,
    )


def make_ollama_client():
    return ollama.Client(host=OLLAMA_HOST)


# ---------------------------------------------------------------------------
# Generic helpers
# ---------------------------------------------------------------------------


def _get(obj: Any, attr: str, default: Any = None) -> Any:
    """Attribute-or-key accessor — works for both pydantic models and dicts."""
    if obj is None:
        return default
    if isinstance(obj, dict):
        return obj.get(attr, default)
    return getattr(obj, attr, default)


def _message_to_dict(msg: Any) -> dict[str, Any]:
    """Coerce an ollama Message (pydantic or dict) to a plain dict for
    the message log we feed back into the next chat() call."""
    if isinstance(msg, dict):
        return {k: v for k, v in msg.items() if v is not None}
    out: dict[str, Any] = {"role": _get(msg, "role") or "assistant"}
    content = _get(msg, "content")
    if content:
        out["content"] = content
    # Preserve `thinking` so the next chat() call sees the model's prior
    # reasoning — important for keeping reasoning models coherent across
    # multi-hop tool loops.
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


# ---------------------------------------------------------------------------
# Public types
# ---------------------------------------------------------------------------


@dataclass
class AgentEvent:
    """One step of the agent loop, surfaced to whichever front-end
    (TUI verbose pane, stdout, log file) is listening."""

    kind: str  # "hop", "search", "results", "synthesis", "error"
    data: dict[str, Any]


EventSink = Callable[[AgentEvent], None]


@dataclass
class ChatState:
    chat_model: str = CHAT_MODEL
    default_k: int = DEFAULT_K
    messages: list[dict[str, Any]] = field(default_factory=list)
    last_sources: list[dict[str, Any]] = field(default_factory=list)

    def reset(self) -> None:
        self.messages = [{"role": "system", "content": SYSTEM_PROMPT}]
        self.last_sources = []


# ---------------------------------------------------------------------------
# Tool: search_parlis
# ---------------------------------------------------------------------------


def search_parlis(
    ollama_client: ollama.Client,
    vectors_client: Any,
    query: str,
    k: int,
) -> dict[str, Any]:
    if not query.strip():
        return {"error": "empty query", "results": []}
    k = max(1, min(int(k), 20))

    emb = ollama_client.embed(model=EMBED_MODEL, input=query)
    vec = None
    embeddings = getattr(emb, "embeddings", None)
    if embeddings is None and isinstance(emb, dict):
        embeddings = emb.get("embeddings")
    if embeddings:
        vec = list(embeddings[0])
    else:
        legacy = (
            getattr(emb, "embedding", None)
            if not isinstance(emb, dict)
            else emb.get("embedding")
        )
        if legacy:
            vec = list(legacy)
    if not vec:
        return {"error": "unexpected ollama embed response shape", "results": []}

    try:
        resp = vectors_client.query_vectors(
            vectorBucketName=BUCKET,
            indexName=INDEX,
            topK=k,
            queryVector={"float32": vec},
            returnDistance=True,
            returnMetadata=True,
        )
    except Exception as e:  # noqa: BLE001 — surface to model
        return {"error": f"QueryVectors failed: {e}", "results": []}

    hits = []
    for v in resp.get("vectors", []):
        meta = v.get("metadata") or {}
        hits.append(
            {
                "source": meta.get("S3VECTORS-EMBED-SRC-LOCATION", v.get("key", "")),
                "chunk_idx": meta.get("S3VECTORS-EMBED-CHUNK-IDX"),
                "snippet": (meta.get("S3VECTORS-EMBED-SRC-CONTENT") or ""),
                "distance": v.get("distance"),
                "key": v.get("key"),
            }
        )
    return {"query": query, "k": k, "results": hits}


# ---------------------------------------------------------------------------
# Agent loop
# ---------------------------------------------------------------------------


def run_turn(
    state: ChatState,
    ollama_client: ollama.Client,
    vectors_client: Any,
    user_text: str,
    on_event: Optional[EventSink] = None,
) -> str:
    """One user-input round-trip. Returns the final answer string.

    `on_event` is called synchronously from the agent loop with structured
    events (hop start, tool call, tool result, synthesis, error). The
    front-end uses it to update verbose / sources panes in real time.
    """

    def emit(kind: str, **data: Any) -> None:
        if on_event is not None:
            try:
                on_event(AgentEvent(kind=kind, data=data))
            except Exception:  # noqa: BLE001 — never let the UI break the agent
                pass

    state.messages.append({"role": "user", "content": user_text})
    state.last_sources = []
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
                    "(model returned an empty response despite "
                    f"{len(state.last_sources)} retrieved sources. Try "
                    "`/reset` or `/model granite4:latest`.)"
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

            if name == "search_parlis":
                query = str(args.get("query") or "")
                k = int(args.get("k") or state.default_k)
                emit("search", query=query, k=k)
                result = search_parlis(ollama_client, vectors_client, query, k)
                state.last_sources.extend(result.get("results", []))
                emit(
                    "results",
                    query=query,
                    hit_count=len(result.get("results", [])),
                    error=result.get("error"),
                    top_distance=(
                        result["results"][0]["distance"]
                        if result.get("results")
                        else None
                    ),
                )
                state.messages.append(
                    {
                        "role": "tool",
                        "name": "search_parlis",
                        "content": json.dumps(result, ensure_ascii=False),
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
        # Hit MAX_TOOL_HOPS — force synthesis on what we have, with tools off.
        emit("synthesis", source_count=len(state.last_sources))
        state.messages.append(
            {
                "role": "user",
                "content": (
                    "You have exhausted your search budget. Produce the "
                    "final answer NOW, based strictly on the search results "
                    "already in your context. Cite source paths in brackets. "
                    "If the evidence is weak or the corpus did not contain a "
                    "clear answer, say so plainly in 1-2 sentences — do not "
                    "invent details and do not ask to search more."
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
                "specific question, or `/model granite4:latest`)"
            )

    emit("final", length=len(final_text), source_count=len(state.last_sources))
    return final_text


# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------


def preflight_index(vectors_client: Any) -> tuple[bool, str]:
    """Cheap GetIndex probe — surface a clear error if the index isn't
    there yet, rather than failing on the first search call."""
    try:
        vectors_client.get_index(vectorBucketName=BUCKET, indexName=INDEX)
        return True, f"index {BUCKET}/{INDEX} reachable"
    except Exception as e:  # noqa: BLE001
        return False, str(e)


def fetch_full_chunk(vectors_client: Any, key: str) -> dict[str, Any]:
    """Pull a single vector's metadata + key by key. Used by the
    TUI's source-preview modal to show the full chunk text rather than
    only the snippet that came back with QueryVectors."""
    try:
        resp = vectors_client.get_vectors(
            vectorBucketName=BUCKET,
            indexName=INDEX,
            keys=[key],
            returnData=False,
            returnMetadata=True,
        )
    except Exception as e:  # noqa: BLE001
        return {"error": str(e)}
    vs = resp.get("vectors", [])
    if not vs:
        return {"error": "vector not found"}
    v = vs[0]
    meta = v.get("metadata") or {}
    return {
        "source": meta.get("S3VECTORS-EMBED-SRC-LOCATION", v.get("key", "")),
        "snippet": meta.get("S3VECTORS-EMBED-SRC-CONTENT") or "",
        "chunk_idx": meta.get("S3VECTORS-EMBED-CHUNK-IDX"),
        "content_hash": meta.get("S3VECTORS-EMBED-CONTENT-HASH"),
        "key": v.get("key"),
    }
