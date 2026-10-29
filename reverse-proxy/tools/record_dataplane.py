"""生成数据面 golden 样例（不消耗账号额度、不出网）。

做法：用 TestClient 打真实 handler，把 httpx.AsyncClient 换成 MockTransport：
  · 记录「客户端请求」与「实际发往上游的请求」；
  · 返回预置的 SSE / 错误体；
  · 记录「回给客户端的响应」。
产物：tests/fixtures/proxy/dataplane/*.json

这些样例是 Rust 重写的对齐目标：同一输入 → 上游请求与客户端输出必须一致
（允许 id / 时间戳差异，比对时按字段规则忽略）。

用法：
    ./.venv/Scripts/python.exe tools/record_dataplane.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import httpx
from fastapi.testclient import TestClient

REPO = Path(__file__).resolve().parents[1]
ROOT = REPO.parent
sys.path.insert(0, str(REPO))

from core import converter  # noqa: E402

OUT = ROOT / "tests" / "fixtures" / "proxy" / "dataplane"

KEEP_HEADERS = (
    "content-type", "accept", "user-agent", "x-product", "x-ide-type",
    "x-ide-name", "x-ide-version", "x-requested-with", "x-codebuddy-request",
    "x-user-id", "x-enterprise-id", "x-tenant-id", "x-domain",
)

CHAT_CLI_HEADERS = {
    "Authorization": "Bearer golden-token",
    "Content-Type": "application/json",
    "Accept": "application/json",
    "User-Agent": "CLI/2.177.1 CodeBuddy/2.177.1",
    "X-Product": "SaaS",
    "X-IDE-Type": "CLI",
    "X-IDE-Name": "CLI",
    "X-IDE-Version": "2.177.1",
    "x-requested-with": "XMLHttpRequest",
    "x-codebuddy-request": "1",
    "X-User-Id": "anonymous",
}

CHAT_DESKTOP_HEADERS = {
    "Authorization": "Bearer golden-token",
    "Content-Type": "application/json",
    "User-Agent": "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1",
}

CHAT_BODY = {
    "model": "deepseek-v4-flash",
    "messages": [
        {"role": "system", "content": "You are an AI agent powered by Golden Harness.\nKeep this rule."},
        {"role": "user", "content": "hi"},
    ],
    "stream": True,
    "stream_options": {"include_usage": True},
    "max_tokens": 64,
    "reasoning_effort": "high",
    "reasoning_summary": "auto",
    "cache_retention": "long",
    "extra_vars": {"harnessMetadata": "{\"schemaVersion\":1}", "initiator": "user"},
}

SSE_OK = (
    b'data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"pong"}}]}\n\n'
    b'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],'
    b'"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}\n\n'
    b"data: [DONE]\n\n"
)

SSE_INCOMPLETE = b'data: {"choices":[{"index":0,"delta":{"content":"half"}}]}\n\n'

ERR_11128 = json.dumps({"code": 11128, "msg": "Illegal API invocation from an unapproved channel"}).encode()


class FakeCred:
    def summary(self):
        return {"uid": "golden-uid", "nickname": "golden", "domain": "www.codebuddy.cn"}

    def get_headers(self, profile="desktop", request_headers=None):
        base = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "Authorization": "Bearer golden-account-token",
            "X-User-Id": "golden-uid",
            "X-Enterprise-Id": "",
            "X-Tenant-Id": "",
            "X-Domain": "www.codebuddy.cn",
            "User-Agent": converter.USER_AGENT,
        }
        return converter.client_profile.merge_outbound_headers(base, request_headers, profile)


def pick(headers: httpx.Headers | dict) -> dict:
    items = headers.items() if hasattr(headers, "items") else headers
    out = {}
    for key, value in items:
        name = key.lower() if isinstance(key, str) else key.decode().lower()
        if name == "authorization":
            out["authorization"] = "Bearer <redacted>"
        elif name in KEEP_HEADERS:
            out[name] = value
    return out


def frames_of(text: str) -> list[str]:
    return [line for line in text.splitlines() if line.startswith("data: ")]


def run_case(name: str, note: str, path: str, headers: dict, body: dict,
             upstream_status: int = 200, upstream_body: bytes = SSE_OK) -> dict:
    captured: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["method"] = request.method
        captured["url"] = str(request.url)
        captured["headers"] = pick(request.headers)
        captured["body"] = request.content.decode("utf-8", "replace")
        return httpx.Response(upstream_status, content=upstream_body,
                              headers={"content-type": "text/event-stream" if upstream_status == 200
                                       else "application/json"})

    original = httpx.AsyncClient
    converter.httpx.AsyncClient = lambda **kw: original(
        transport=httpx.MockTransport(handler), **kw)
    converter._check_auth = lambda *a, **k: None
    converter.CONFIG["cred"] = FakeCred()
    try:
        response = TestClient(converter.app).post(path, headers=headers, json=body)
    finally:
        converter.httpx.AsyncClient = original

    text = response.text
    return {
        "name": name,
        "note": note,
        "request": {"method": "POST", "path": path, "headers": pick(headers), "body": body},
        "upstream": captured,
        "upstream_response": {"status": upstream_status,
                              "body": upstream_body.decode("utf-8", "replace")},
        "response": {
            "status": response.status_code,
            "content_type": response.headers.get("content-type", ""),
            "body": text,
            "data_frames": frames_of(text),
        },
    }


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    cases = []

    cases.append(run_case(
        "chat_cli", "CLI 流量：透传指纹头 + CLI 独有字段 + 11128 提示词改写",
        "/v1/chat/completions", CHAT_CLI_HEADERS, CHAT_BODY))
    cases.append(run_case(
        "chat_desktop", "桌面端流量：保持旧行为（桌面 UA、丢弃 CLI 字段）",
        "/v1/chat/completions", CHAT_DESKTOP_HEADERS, CHAT_BODY))
    cases.append(run_case(
        "responses_stream", "Codex 的 /v1/responses：chat SSE → Responses SSE",
        "/v1/responses", {"Authorization": "Bearer golden-token", "Content-Type": "application/json"},
        {"model": "deepseek-v4-flash", "input": "hi", "stream": True}))
    cases.append(run_case(
        "anthropic_stream", "Claude Code 的 /v1/messages：chat SSE → Anthropic SSE",
        "/v1/messages", {"Authorization": "Bearer golden-token", "Content-Type": "application/json"},
        {"model": "deepseek-v4-flash", "max_tokens": 64,
         "messages": [{"role": "user", "content": "hi"}], "stream": True}))
    cases.append(run_case(
        "chat_error_11128", "上游 400 / code 11128：客户端应收到可读错误与 wb_kind",
        "/v1/chat/completions", CHAT_CLI_HEADERS, dict(CHAT_BODY, stream=False),
        upstream_status=400, upstream_body=ERR_11128))
    cases.append(run_case(
        "chat_incomplete_stream", "上游没有终态帧：必须报 incomplete_stream，不能当成功",
        "/v1/chat/completions", CHAT_CLI_HEADERS, dict(CHAT_BODY, stream=True),
        upstream_body=SSE_INCOMPLETE))

    index = []
    for case in cases:
        path = OUT / f"{case['name']}.json"
        path.write_text(json.dumps(case, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        index.append({"name": case["name"], "note": case["note"]})
    (OUT / "index.json").write_text(
        json.dumps({"cases": index, "generated_by": "reverse-proxy/tools/record_dataplane.py"},
                   ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"cases={len(cases)} -> {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
