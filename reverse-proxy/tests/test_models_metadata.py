"""G7：/v1/models 要带能力元数据，并补国内版种子。"""

from fastapi.testclient import TestClient

from core import converter


def test_metadata_reports_images_and_reasoning():
    entry = {
        "id": "glm-5.3", "name": "GLM 5.3",
        "maxInputTokens": 1_000_000, "maxOutputTokens": 65_536,
        "supportsImages": True, "supportsReasoning": True, "supportsToolCall": True,
        "reasoning": {"supportedEfforts": ["low", "high"], "canDisableThinking": True},
        "tags": ["限时免费"],
    }
    model = converter.model_metadata_entry(entry)
    assert model["input_modalities"] == ["text", "image"]
    assert model["context_window"] == 1_000_000
    assert model["max_output_tokens"] == 65_536
    assert model["reasoning_efforts"] == ["low", "high"]
    assert model["source"] == "catalogue"
    assert model["tags"] == ["限时免费"]


def test_disabled_multimodal_wins_over_supports_images():
    entry = {"id": "x", "supportsImages": True, "disabledMultimodal": True}
    assert converter.model_metadata_entry(entry)["input_modalities"] == ["text"]


def test_suspicious_context_falls_back_to_default():
    entry = {"id": "x", "maxInputTokens": 128}   # 上游偶发把 KB 写成 token
    assert converter.model_metadata_entry(entry)["context_window"] == converter.DEFAULT_CONTEXT_WINDOW


def test_seeds_fill_gaps_for_cn(monkeypatch):
    monkeypatch.setattr(converter, "_cred", lambda: (_ for _ in ()).throw(converter.HTTPException(503, "x")))
    monkeypatch.setattr(converter, "get_model_catalog_for", lambda realm, refresh=False: [{"id": "glm-5.3"}])
    models = converter.list_model_metadata()
    ids = {m["id"] for m in models}
    assert "glm-5.3" in ids and "deepseek-v4-pro" in ids
    seed = next(m for m in models if m["id"] == "deepseek-v4-pro")
    assert seed["source"] == "seed"
    assert seed["input_modalities"] == ["text"]
    # 目录里已有的 id 不被种子覆盖
    assert sum(1 for m in models if m["id"] == "glm-5.3") == 1


def test_no_seeds_for_global(monkeypatch):
    monkeypatch.setattr(converter, "_cred", lambda: None)
    monkeypatch.setattr(converter, "_catalog_realm_or_cn", lambda cred: "global")
    monkeypatch.setattr(converter, "get_model_catalog_for", lambda realm, refresh=False: [{"id": "gpt-5.6-sol"}])
    ids = {m["id"] for m in converter.list_model_metadata()}
    assert ids == {"gpt-5.6-sol"}


def test_models_route_exposes_metadata(monkeypatch):
    monkeypatch.setattr(converter, "_check_auth", lambda *a, **k: None)
    monkeypatch.setattr(converter, "_cred", lambda: None)
    monkeypatch.setattr(converter, "_catalog_realm_or_cn", lambda cred: "cn")
    monkeypatch.setattr(converter, "get_model_catalog_for", lambda realm, refresh=False: [{
        "id": "m1", "supportsImages": True, "maxInputTokens": 200_000,
    }])
    response = TestClient(converter.app).get("/v1/models")
    assert response.status_code == 200
    first = response.json()["data"][0]
    assert first["id"] == "m1"
    assert first["input_modalities"] == ["text", "image"]
    assert first["context_window"] == 200_000
