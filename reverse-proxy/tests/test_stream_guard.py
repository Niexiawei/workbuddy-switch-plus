"""G5：流空闲 / 无终态 / 空回答必须报错，不能当成功。"""

import asyncio
import json

import httpx

from core import converter


def _patch_transport(monkeypatch, body: bytes, delay: float = 0.0):
    original = httpx.AsyncClient

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=body)

    monkeypatch.setattr(
        converter.httpx, "AsyncClient",
        lambda **kw: original(transport=httpx.MockTransport(handler), **kw),
    )


def _collect(monkeypatch, body: bytes, model: str = "m") -> bytes:
    _patch_transport(monkeypatch, body)

    async def run() -> bytes:
        out = b""
        async for chunk in converter._stream_upstream(
            "http://upstream/v2/chat/completions", {}, {"stream": True}, model, 0.0, ""
        ):
            out += chunk
        return out

    return asyncio.run(run())


def test_stream_without_terminal_frame_reports_incomplete(monkeypatch):
    # 只有内容分片，没有 finish_reason，也没有 [DONE]
    body = b'data: {"choices":[{"delta":{"content":"half"}}]}\n\n'
    out = _collect(monkeypatch, body)
    assert b"half" in out
    frames = [json.loads(line[5:]) for line in out.splitlines() if line.startswith(b"data: {")]
    assert any(f.get("error", {}).get("type") == "incomplete_stream" for f in frames)


def test_empty_stream_reports_empty_response(monkeypatch):
    out = _collect(monkeypatch, b"")
    frames = [json.loads(line[5:]) for line in out.splitlines() if line.startswith(b"data: {")]
    assert any(f.get("error", {}).get("type") == "empty_response" for f in frames)


def test_complete_stream_has_no_error_frame(monkeypatch):
    body = (
        b'data: {"choices":[{"delta":{"content":"ok"}}]}\n\n'
        b'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\n'
        b"data: [DONE]\n\n"
    )
    out = _collect(monkeypatch, body)
    assert b"error" not in out.lower()


def test_upstream_error_body_is_classified(monkeypatch):
    original = httpx.AsyncClient

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(400, content=b'{"code":11128,"msg":"channel check"}')

    monkeypatch.setattr(
        converter.httpx, "AsyncClient",
        lambda **kw: original(transport=httpx.MockTransport(handler), **kw),
    )

    async def run() -> bytes:
        out = b""
        async for chunk in converter._stream_upstream(
            "http://upstream/v2/chat/completions", {}, {}, "m", 0.0, ""
        ):
            out += chunk
        return out

    out = asyncio.run(run())
    payload = json.loads(out.decode().split("data: ", 1)[1])
    assert payload["error"]["code"] == 11128
    assert payload["error"]["type"] == "bad_request"
