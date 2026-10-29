"""默认模型：客户端没带 `model` 时，反代替它选的那一个。

上游的行为是把 `"auto"` 透给后端（由后端自己挑）；二改后改成
`converter.DEFAULT_MODEL`（deepseek-v4.1-flash），理由是默认值可预测——
客户端日志里看到的就是真实在跑的模型，排查「对不上号」时不用再猜后端挑了哪个。

但可用清单来自**本机 WorkBuddy 的 product.json**，各客户端版本内容不同，
所以配置值必须经过一层运行时解析：本机没有这个模型就退回 `auto`，
否则所有「客户端没带 model」的请求都会带着未知模型打上游（整片失败），
后台测试表单也会选不中。

这个值有三处出口，这里都锁住：
  1. 请求体回退（`core/converter.py` 的 `setdefault("model", ...)`）；
  2. 管理后台的测试接口默认值（`admin/server.py`）；
  3. `/admin/api/overview` 的 `default_model` 字段（桌面端与网页后台的表单默认选中项）。
"""

import asyncio
import json
import tempfile
import time
import unittest
from pathlib import Path

import httpx
import pytest

import admin.server as admin_server
from core import converter

ADMIN = "admin-test-credential-long-enough"


def _resolve_with(monkeypatch, models):
    """在「本机可用清单 = models」的前提下解析默认模型。"""
    monkeypatch.setattr(converter, "_default_model_cache", None)
    monkeypatch.setattr(converter, "get_available_models", lambda: list(models))
    return converter.resolve_default_model()


def test_bundled_fallback_list_contains_the_configured_default():
    """product.json 读不到时会退到 DEFAULT_MODELS，配置值必须在里面，否则清单自相矛盾。"""
    assert converter.DEFAULT_MODEL in converter.DEFAULT_MODELS


def test_uses_configured_default_when_present(monkeypatch):
    value = _resolve_with(monkeypatch, [converter.DEFAULT_MODEL, "glm-5.2"])
    assert value == converter.DEFAULT_MODEL


def test_falls_back_when_local_list_lacks_it(monkeypatch):
    # 这正是本机的实际情况：WorkBuddy 的清单里有 deepseek-v4-flash，没有 .1 那一版。
    value = _resolve_with(monkeypatch, ["default", "deepseek-v4-flash", "glm-5.2", "auto"])
    assert value == converter.FALLBACK_MODEL == "auto"


def test_empty_list_keeps_configured_default(monkeypatch):
    """清单为空 = 没读到 product.json。没有依据判定可用性，就按配置值走。"""
    value = _resolve_with(monkeypatch, [])
    assert value == converter.DEFAULT_MODEL


def test_resolution_is_cached(monkeypatch):
    """清单来自文件，不该每次请求都去读一遍。"""
    monkeypatch.setattr(converter, "_default_model_cache", None)
    calls = []

    def counting():
        calls.append(1)
        return [converter.DEFAULT_MODEL]

    monkeypatch.setattr(converter, "get_available_models", counting)
    assert converter.resolve_default_model() == converter.DEFAULT_MODEL
    assert converter.resolve_default_model() == converter.DEFAULT_MODEL
    assert len(calls) == 1


@pytest.mark.parametrize("path", ["/v1/chat/completions", "/v1/responses"])
@pytest.mark.parametrize("available", [True, False])
def test_omitted_model_uses_resolved_default(monkeypatch, path, available):
    """客户端不带 model 时，发往上游的 body 里必须是解析后的默认模型。"""
    models = ["glm-5.2", converter.DEFAULT_MODEL] if available else ["glm-5.2", "auto"]
    expected = _resolve_with(monkeypatch, models)

    original_client = httpx.AsyncClient
    captured = []

    def upstream(request):
        captured.append(json.loads(request.content))
        payload = b'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
        if path.endswith("responses"):
            payload = b'data: {"type":"response.output_text.delta","delta":"ok"}\n\ndata: [DONE]\n\n'
        return httpx.Response(200, content=payload)

    monkeypatch.setattr(
        converter.httpx, "AsyncClient",
        lambda **kw: original_client(transport=httpx.MockTransport(upstream), **kw),
    )
    monkeypatch.setattr(converter, "_check_auth", lambda *a: None)
    monkeypatch.setattr(
        converter, "_cred",
        lambda: type("Credential", (), {"get_headers": lambda self, **kwargs: {}})(),
    )
    monkeypatch.setattr(converter, "_log", lambda *a: None)
    monkeypatch.delenv("CODEBUDDY_LOSSY_PROJECTION", raising=False)
    monkeypatch.delenv("CODEBUDDY_RESPONSES_DESENSITIZE", raising=False)

    body = {"messages": [{"role": "user", "content": "hi"}], "stream": False}
    if path.endswith("responses"):
        body = {"input": "hi", "stream": False}

    async def run():
        async with original_client(
            transport=httpx.ASGITransport(app=converter.app), base_url="http://test"
        ) as client:
            response = await client.post(path, json=body)
            assert response.status_code == 200

    asyncio.run(run())
    assert captured, f"{path} 没有发出上游请求"
    assert captured[0]["model"] == expected


class OverviewDefaultModelTests(unittest.IsolatedAsyncioTestCase):
    """`/admin/api/overview` 要把默认模型带出去——界面靠它决定表单默认选中项。"""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        auth = root / "auth"
        auth.mkdir()
        (auth / "session.info").write_text(
            json.dumps({
                "account": {"uid": "one", "nickname": "测试账号", "enterpriseId": "test"},
                "auth": {
                    "accessToken": "private-access-token",
                    "refreshToken": "private-refresh-token",
                    "expiresAt": int(time.time() * 1000) + 3600000,
                },
            }),
            encoding="utf-8",
        )
        self.app = admin_server.create_app(root / "management", auth, "client-key", ADMIN)
        self.client = httpx.AsyncClient(
            transport=httpx.ASGITransport(app=self.app), base_url="https://console.test"
        )

    async def asyncTearDown(self):
        await self.client.aclose()
        self.temp.cleanup()

    async def test_overview_default_model_is_usable_by_the_form(self):
        login = await self.client.post("/admin/api/login", json={"key": ADMIN})
        self.assertEqual(login.status_code, 200)
        overview = (await self.client.get("/admin/api/overview")).json()
        # 界面拿这个值去 `models` 里选中；选不中就会退回清单首项，用户看到的是别的模型。
        self.assertIn(overview["default_model"], overview["models"])
        self.assertEqual(overview["default_model"], converter.resolve_default_model())
