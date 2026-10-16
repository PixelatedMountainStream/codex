#!/usr/bin/env python3
"""Minimal mock Ollama + OpenAI Responses server (stdlib) for offline verification.

Serves: GET /api/tags, /api/version, /v1/models; POST /api/show; POST /v1/responses
(SSE stream, or JSON when stream is false) with one fixed text reply.

Scope: the minimum event sequence for a single text reply (created, output_item.added,
output_text.delta, output_item.done, completed with usage). No tool calls, no chat
completions. Validated against a real codex-local build (verify_install --quick).
"""

from __future__ import annotations

import argparse
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL = "qwen3.8:27b"
REPLY = "MOCK-OK"


def _sse(event: str, data: dict) -> bytes:
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


def responses_events(text: str = REPLY, model: str = MODEL) -> list[bytes]:
    rid, mid = "resp_mock", "msg_mock"
    msg = {"type": "message", "role": "assistant", "id": mid,
           "content": [{"type": "output_text", "text": text}]}
    usage = {"input_tokens": 1, "input_tokens_details": None,
             "output_tokens": 1, "output_tokens_details": None, "total_tokens": 2}
    return [
        _sse("response.created", {"type": "response.created",
                                  "response": {"id": rid, "model": model}}),
        _sse("response.output_item.added", {"type": "response.output_item.added",
             "output_index": 0, "item": {**msg, "content": []}}),
        _sse("response.output_text.delta", {"type": "response.output_text.delta",
             "item_id": mid, "output_index": 0, "content_index": 0, "delta": text}),
        _sse("response.output_item.done", {"type": "response.output_item.done",
                                           "output_index": 0, "item": msg}),
        _sse("response.completed", {"type": "response.completed",
                                    "response": {"id": rid, "model": model, "usage": usage}}),
    ]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # silence
        pass

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/api/tags":
            self._json({"models": [{"name": MODEL, "model": MODEL}]})
        elif p == "/api/version":
            self._json({"version": "0.0.0-mock"})
        elif p == "/v1/models":
            self._json({"object": "list", "data": [{"id": MODEL, "object": "model"}]})
        else:
            self._json({"error": "not found"}, 404)

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b"{}"
        try:
            req = json.loads(raw or b"{}")
        except ValueError:
            req = {}
        p = self.path.split("?")[0]
        if p == "/api/show":
            self._json({"capabilities": ["completion", "tools"], "details": {},
                        "model_info": {"general.architecture": "mock",
                                       "mock.context_length": 32768}})
        elif p == "/v1/responses":
            model = req.get("model") or MODEL
            events = responses_events(model=model)
            if req.get("stream") is False:
                last = json.loads(events[-1].split(b"data: ", 1)[1])
                self._json(last["response"])
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            for e in events:
                self.wfile.write(e)
            self.wfile.flush()
            self.close_connection = True
        else:
            self._json({"error": "not found"}, 404)


def start(port: int = 0) -> tuple[ThreadingHTTPServer, str]:
    """Start on 127.0.0.1 in a daemon thread; returns (server, base_url)."""
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=0)
    a = ap.parse_args()
    s, url = start(a.port)
    print(url, flush=True)
    threading.Event().wait()
