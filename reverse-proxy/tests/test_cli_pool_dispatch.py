"""池调度层夹具：CLI 流量经 PoolMiddleware 选号 → 上游拿到 CLI 指纹与 CLI 字段。

之前的验证只打到了 `chat_completions` handler（绕过选号）。这里补上真实
`create_app` + `SwitchStore`，让 `PoolMiddleware` 真的跑一遍：密钥校验、选号、
把凭据绑进 `REQUEST_CREDENTIAL`。上游用 `httpx.MockTransport` 拦截，不出网。

夹具要点（否则假账号会被冷却 → 503「暂无可用账号」）：
  · 关掉 `auto_checkin` / `auto_travel` / `auto_keepalive` / `auto_active`，
    假账号没有真实凭据，任何一次真实 billing 请求都会 401 并把账号冷却 5 分钟；
  · 不进 `with TestClient(...)`，避免 lifespan 里的池后台线程参与。
"""

import json

import httpx
import pytest
from fastapi.testclient import TestClient

from admin import server as admin_server
from admin.pool import DEFAULT_POOL
from admin.switch_store import SwitchStore
from core import converter

CLI_HEADERS = {
    "Authorization": "Bearer probe-key",
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

DESKTOP_HEADERS = {
    "Authorization": "Bearer probe-key",
    "Content-Type": "application/json",
    "User-Agent": "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1",
}

BODY = {
    "model": "deepseek-v4-flash",
    "messages": [{"role": "user", "content": "hi"}],
    "stream": True,
    "max_tokens": 64,
    "reasoning_effort": "high",
    "reasoning_summary": "auto",
    "cache_retention": "long",
    "extra_vars": {"harnessMetadata": "{\"schemaVersion\":1}", "initiator": "user"},
}

SSE = (
    b'data: {"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\n'
    b"data: [DONE]\n\n"
)


@pytest.fixture()
def app_and_capture(tmp_path):
    accounts = tmp_path / "accounts.json"
    accounts.write_text(json.dumps([{
        "id": "probe1",
        "uid": "uid-probe",
        "nickname": "probe",
        "access_token": "fake-account-token",
        "expiresAt": 4102444800000,
        "domain": "www.codebuddy.cn",
        "variant": "cn",
        "enterpriseId": "ent-probe",
    }], ensure_ascii=False), encoding="utf-8")

    store = SwitchStore(root=str(tmp_path / "state"), accounts_path=accounts,
                        initial_key="probe-key", admin_key="a" * 40)
    store.data["pool"] = {**DEFAULT_POOL, "auto_checkin": False, "auto_travel": False,
                          "auto_keepalive": False, "auto_active": False}
    store.save()
    app = admin_server.create_app(store=store, admin_key="a" * 40)

    captured = []

    def upstream(request: httpx.Request) -> httpx.Response:
        captured.append(request)
        return httpx.Response(200, content=SSE)

    original = converter.httpx.AsyncClient
    converter.httpx.AsyncClient = lambda **kw: original(
        transport=httpx.MockTransport(upstream), **kw
    )
    try:
        yield TestClient(app), captured
    finally:
        converter.httpx.AsyncClient = original
        store.stop_runtime_writer()


def test_cli_traffic_keeps_client_fingerprint(app_and_capture):
    client, captured = app_and_capture
    response = client.post("/v1/chat/completions", headers=CLI_HEADERS, json=BODY)
    assert response.status_code == 200

    request = captured[-1]
    assert request.url.path.endswith("/v2/chat/completions")
    # G2：CLI 指纹头透传，鉴权换成账号 token
    assert request.headers["user-agent"] == "CLI/2.177.1 CodeBuddy/2.177.1"
    assert request.headers["x-product"] == "SaaS"
    assert request.headers["x-ide-version"] == "2.177.1"
    assert request.headers["authorization"] == "Bearer fake-account-token"
    # G3：身份头仍以账号为准
    assert request.headers["x-user-id"] == "uid-probe"


def test_cli_traffic_keeps_extra_body_fields(app_and_capture):
    client, captured = app_and_capture
    assert client.post("/v1/chat/completions", headers=CLI_HEADERS, json=BODY).status_code == 200
    body = json.loads(captured[-1].content)
    # G1：extra_vars / cache_retention 不能丢
    assert body["extra_vars"]["initiator"] == "user"
    assert body["cache_retention"] == "long"
    assert body["reasoning_summary"] == "auto"


def test_desktop_traffic_unchanged(app_and_capture):
    client, captured = app_and_capture
    assert client.post("/v1/chat/completions", headers=DESKTOP_HEADERS, json=BODY).status_code == 200
    request = captured[-1]
    body = json.loads(request.content)
    assert request.headers["user-agent"] == converter.USER_AGENT
    assert "x-product" not in request.headers
    assert "extra_vars" not in body and "cache_retention" not in body
