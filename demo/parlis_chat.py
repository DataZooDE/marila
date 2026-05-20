#!/usr/bin/env python3
"""
Agentic RAG chat over a marila vector index.

Defaults to talking to the corpus that `demo/index_parlis.sh` indexed
(German parliamentary PDFs), but `BUCKET` / `INDEX` env vars retarget it
at any marila index.

How the agent works
-------------------
Local Ollama hosts BOTH the embedding model (embeddinggemma) and the
chat model (default `gpt-oss:latest` — supports tool calling). The chat
model is given exactly one tool, `search_parlis`. The agent loop:

  1. User types a question.
  2. The chat model decides on its own whether to call the search tool,
     how to phrase the query (it may translate, expand, or split into
     sub-queries), and how many times to call it.
  3. Each call: embed the query via embeddinggemma → QueryVectors on
     marila → return top-k hits with source path, snippet, distance.
  4. The model loops on its own tool calls until it has enough context,
     then writes a final answer with `[source]` citations.

The whole conversation history persists across turns so follow-ups
(e.g. "and the Bundestag's response?") see the prior answer.

Usage
-----
  source demo/.venv/bin/activate         # or `uv venv` then `uv pip install -e demo/`
  python demo/parlis_chat.py

Slash commands
--------------
  /sources       Print sources cited by the last assistant turn.
  /reset         Wipe conversation history.
  /verbose       Toggle showing tool calls + tool results inline.
  /model NAME    Switch chat model (must support `tools` capability).
  /k N           Change default top-k for new searches.
  /quit          Exit.
"""

from __future__ import annotations

import json
import os
import sys
import textwrap
from dataclasses import dataclass, field
from typing import Any

import boto3
import ollama

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

MARILA_ENDPOINT = os.environ.get("MARILA_ENDPOINT", "http://localhost:8080")
MARILA_REGION = os.environ.get("MARILA_REGION", "eu-west-1")
MARILA_ACCESS_KEY = os.environ.get("MARILA_ACCESS_KEY_ID", "marila")
MARILA_SECRET = os.environ.get("MARILA_SECRET_ACCESS_KEY", "marilasecret")

BUCKET = os.environ.get("BUCKET", "parlis")
INDEX = os.environ.get("INDEX", "drucksachen")

OLLAMA_HOST = os.environ.get("OLLAMA_ENDPOINT", "http://localhost:11434")
EMBED_MODEL = os.environ.get("EMBED_MODEL", "embeddinggemma:latest")
CHAT_MODEL = os.environ.get("CHAT_MODEL", "gpt-oss:latest")

# Agent loop safety cap — pathological models can keep calling the tool
# forever. 50 is generous: deep reasoning chains over the parlis corpus
# can legitimately need many refinements. Override with the env var if
# you want a tighter ceiling.
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
                            "max 20). Increase when you need more context, "
                            "decrease for tighter relevance."
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
# ANSI helpers (no rich dep, no fuss)
# ---------------------------------------------------------------------------


def _colour(text: str, code: str) -> str:
    if not sys.stdout.isatty():
        return text
    return f"\x1b[{code}m{text}\x1b[0m"


def dim(s: str) -> str:
    return _colour(s, "2")


def bold(s: str) -> str:
    return _colour(s, "1")


def cyan(s: str) -> str:
    return _colour(s, "36")


def green(s: str) -> str:
    return _colour(s, "32")


def yellow(s: str) -> str:
    return _colour(s, "33")


def red(s: str) -> str:
    return _colour(s, "31")


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
# Tool implementation
# ---------------------------------------------------------------------------


@dataclass
class ChatState:
    """Mutable per-process state — model, verbosity, conversation log."""

    chat_model: str = CHAT_MODEL
    default_k: int = DEFAULT_K
    verbose: bool = False
    messages: list[dict[str, Any]] = field(default_factory=list)
    last_sources: list[dict[str, Any]] = field(default_factory=list)

    def reset(self) -> None:
        self.messages = [{"role": "system", "content": SYSTEM_PROMPT}]
        self.last_sources = []


def search_parlis(
    ollama_client: ollama.Client,
    vectors_client: Any,
    query: str,
    k: int,
) -> dict[str, Any]:
    """Embed `query` via embeddinggemma, QueryVectors against marila, return hits."""
    if not query.strip():
        return {"error": "empty query", "results": []}
    k = max(1, min(int(k), 20))

    emb = ollama_client.embed(model=EMBED_MODEL, input=query)
    # ollama-python returns either a pydantic `EmbedResponse` (current)
    # or a plain dict (older). Normalise both to a flat list.
    vec = None
    embeddings = getattr(emb, "embeddings", None)
    if embeddings is None and isinstance(emb, dict):
        embeddings = emb.get("embeddings")
    if embeddings:
        vec = list(embeddings[0])
    else:
        legacy = getattr(emb, "embedding", None) if not isinstance(emb, dict) else emb.get("embedding")
        if legacy:
            vec = list(legacy)
    if not vec:
        return {"error": f"unexpected ollama embed response shape", "results": []}
    qvec = vec

    try:
        resp = vectors_client.query_vectors(
            vectorBucketName=BUCKET,
            indexName=INDEX,
            topK=k,
            queryVector={"float32": qvec},
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
                "snippet": (meta.get("S3VECTORS-EMBED-SRC-CONTENT") or "")[:600],
                "distance": v.get("distance"),
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
) -> str:
    state.messages.append({"role": "user", "content": user_text})
    state.last_sources = []
    final_text = ""

    for hop in range(MAX_TOOL_HOPS):
        if state.verbose:
            print(dim(f"  [hop {hop + 1}/{MAX_TOOL_HOPS}] calling {state.chat_model}…"))
        resp = ollama_client.chat(
            model=state.chat_model,
            messages=state.messages,
            tools=TOOL_DEFINITIONS,
        )
        msg = getattr(resp, "message", None)
        if msg is None and isinstance(resp, dict):
            msg = resp.get("message")
        if msg is None:
            msg = {}
        content = _get(msg, "content") or ""
        # `thinking` is the chain-of-thought channel on reasoning-capable
        # models (gpt-oss, granite4, …). Most of the time it's just
        # internal and the user-facing answer goes in `content`; but
        # gpt-oss in particular sometimes drops `content` after many
        # tool round-trips and leaves the entire output in `thinking`.
        # We capture it as a fallback below.
        thinking = _get(msg, "thinking") or ""
        tool_calls = _get(msg, "tool_calls") or []

        if state.verbose:
            print(
                dim(
                    f"  ← content={len(content)}ch  thinking={len(thinking)}ch  "
                    f"tool_calls={len(tool_calls)}"
                )
            )

        # Persist the assistant turn (with whatever fields the model
        # returned — Ollama-python returns Message objects that
        # serialise cleanly under json.dumps).
        state.messages.append(_message_to_dict(msg))

        if not tool_calls:
            if content.strip():
                final_text = content
            elif thinking.strip():
                final_text = (
                    "(model emitted no `content` channel on the final turn — "
                    "falling back to its reasoning channel)\n\n"
                    + thinking
                )
            else:
                final_text = (
                    "(model returned an empty response despite "
                    f"{len(state.last_sources)} retrieved sources. "
                    "Try `/reset` and rephrase, or `/model granite4:latest` / "
                    "`/model mistral:latest` — both tend to be steadier on "
                    "German + tool-use than gpt-oss.)"
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
                if state.verbose:
                    print(dim(f"  ↪ search_parlis(query={query!r}, k={k})"))
                result = search_parlis(ollama_client, vectors_client, query, k)
                # Track sources for /sources command.
                state.last_sources.extend(result.get("results", []))
                state.messages.append(
                    {
                        "role": "tool",
                        "name": "search_parlis",
                        "content": json.dumps(result, ensure_ascii=False),
                    }
                )
                if state.verbose:
                    for r in result.get("results", [])[:3]:
                        print(
                            dim(
                                f"     • {r.get('source')} (d={r.get('distance'):.4f})"
                                if isinstance(r.get("distance"), (int, float))
                                else f"     • {r.get('source')}"
                            )
                        )
            else:
                state.messages.append(
                    {
                        "role": "tool",
                        "name": name or "unknown",
                        "content": json.dumps({"error": f"unknown tool {name!r}"}),
                    }
                )
    else:
        # Hit the tool-hop cap without the model writing a final answer.
        # Rather than discard all the search results we accumulated,
        # force a synthesis turn: one more chat() call with tools
        # disabled, asking the model to commit to an answer based on
        # what's already in its context window.
        if state.verbose:
            print(
                dim(
                    f"  [synthesis] tool budget exhausted — forcing one "
                    f"no-tools call to commit to an answer over "
                    f"{len(state.last_sources)} retrieved sources"
                )
            )
        state.messages.append(
            {
                "role": "user",
                "content": (
                    "You have exhausted your search budget. Produce the "
                    "final answer NOW, based strictly on the search results "
                    "already in your context. Cite source paths in brackets, "
                    "e.g. `[17_1234_D.pdf]`. If the evidence is weak or the "
                    "corpus did not contain a clear answer, say so plainly "
                    "in 1-2 sentences — do not invent details and do not "
                    "ask to search more."
                ),
            }
        )
        resp = ollama_client.chat(
            model=state.chat_model,
            messages=state.messages,
            tools=[],  # critical: no more tool calls
        )
        msg = getattr(resp, "message", None) or (
            resp.get("message") if isinstance(resp, dict) else None
        ) or {}
        state.messages.append(_message_to_dict(msg))
        content = _get(msg, "content") or ""
        thinking = _get(msg, "thinking") or ""
        if content.strip():
            final_text = content
        elif thinking.strip():
            final_text = (
                "(synthesis turn produced no `content` channel — falling "
                "back to thinking)\n\n" + thinking
            )
        else:
            final_text = (
                "(agent burned its tool budget AND the synthesis turn "
                "produced an empty response — try `/reset` and a more "
                "specific question, or `/model granite4:latest`)"
            )

    return final_text


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
# REPL
# ---------------------------------------------------------------------------


def handle_slash(state: ChatState, line: str) -> bool:
    """Handle `/cmd …`. Returns True if the command should exit the REPL."""
    parts = line.strip().split(maxsplit=1)
    cmd = parts[0]
    arg = parts[1] if len(parts) > 1 else ""
    if cmd in ("/quit", "/exit"):
        return True
    if cmd == "/reset":
        state.reset()
        print(dim("(conversation reset)"))
    elif cmd == "/sources":
        if not state.last_sources:
            print(dim("(no sources from the last turn)"))
        else:
            for r in state.last_sources:
                dist = r.get("distance")
                dist_s = f" d={dist:.4f}" if isinstance(dist, (int, float)) else ""
                snippet = (r.get("snippet") or "").replace("\n", " ")[:140]
                print(f"  {cyan(str(r.get('source', '?')))}{dim(dist_s)}  {snippet}")
    elif cmd == "/verbose":
        state.verbose = not state.verbose
        print(dim(f"(verbose = {state.verbose})"))
    elif cmd == "/model":
        if arg:
            state.chat_model = arg.strip()
            print(dim(f"(chat model = {state.chat_model})"))
        else:
            print(dim(f"current chat model: {state.chat_model}"))
    elif cmd == "/k":
        try:
            state.default_k = max(1, int(arg))
            print(dim(f"(default k = {state.default_k})"))
        except ValueError:
            print(red("usage: /k <int>"))
    elif cmd == "/help":
        print(
            dim(
                "  /sources  show citations from the last answer\n"
                "  /reset    wipe history\n"
                "  /verbose  toggle showing tool calls\n"
                "  /model X  switch chat model (must support tools)\n"
                "  /k N      change default top-K for new searches\n"
                "  /quit     exit"
            )
        )
    else:
        print(red(f"unknown command: {cmd} — try /help"))
    return False


def main() -> int:
    print(bold("marila — agentic RAG chat") + dim(f"  ({BUCKET}/{INDEX})"))
    print(
        dim(
            f"  embed: {EMBED_MODEL}   chat: {CHAT_MODEL}   ollama: {OLLAMA_HOST}\n"
            f"  type a question, or /help. Ctrl-D to exit.\n"
        )
    )

    ollama_client = make_ollama_client()
    vectors_client = make_vectors_client()

    # Pre-flight: confirm index exists. A hard fail here is much less
    # confusing than a model that loops 8 times getting 404s.
    try:
        vectors_client.get_index(vectorBucketName=BUCKET, indexName=INDEX)
    except Exception as e:  # noqa: BLE001
        print(red(f"could not reach {BUCKET}/{INDEX}: {e}"))
        print(
            dim(
                "  run `demo/index_parlis.sh` first, or set BUCKET/INDEX env to "
                "an existing marila index."
            )
        )
        return 2

    state = ChatState()
    state.reset()

    while True:
        try:
            line = input(green("\nyou> "))
        except (EOFError, KeyboardInterrupt):
            print()
            break
        if not line.strip():
            continue
        if line.startswith("/"):
            if handle_slash(state, line):
                break
            continue
        try:
            answer = run_turn(state, ollama_client, vectors_client, line)
        except Exception as e:  # noqa: BLE001 — keep the REPL alive
            print(red(f"error: {e}"))
            continue
        print()
        print(bold("assistant>"))
        print(answer)
        if state.last_sources:
            print(dim(f"  ({len(state.last_sources)} sources — type /sources to list)"))

    return 0


if __name__ == "__main__":
    sys.exit(main())
