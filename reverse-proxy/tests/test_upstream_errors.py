"""业务码 → 失败类型 → 客户端状态码 / 冷却维度。"""

from core import upstream_errors as ue


def test_channel_check_is_not_retryable():
    f = ue.classify(400, '{"code":11128,"msg":"Illegal API invocation"}')
    assert f.kind == ue.BAD_REQUEST
    assert f.http_status == 400
    assert f.retryable is False
    assert f.account_cooldown == 0 and f.model_cooldown == 0
    assert "11128" in (f.hint or "")


def test_missing_model_cools_only_that_model():
    f = ue.classify(400, '{"code":11102,"msg":"model not found"}')
    assert f.kind == ue.NO_SUCH_MODEL
    assert f.http_status == 404
    assert f.model_cooldown > 0
    assert f.account_cooldown == 0
    assert f.retryable is False


def test_gateway_rate_limit_cools_model_and_is_retryable():
    f = ue.classify(400, '{"code":11134,"msg":"rate limited"}')
    assert f.kind == ue.RATE_LIMIT
    assert f.http_status == 429
    assert f.model_cooldown > 0
    assert f.retryable is True


def test_auth_status_cools_account():
    f = ue.classify(401, "")
    assert f.kind == ue.AUTH
    assert f.account_cooldown == 300
    assert f.retryable is True


def test_quota_status():
    f = ue.classify(402, '{"msg":"no credits"}')
    assert f.kind == ue.QUOTA
    assert f.http_status == 429
    assert f.account_cooldown > 0


def test_upstream_5xx_is_unavailable():
    f = ue.classify(503, "gateway down")
    assert f.kind == ue.UNAVAILABLE
    assert f.http_status == 502
    assert f.message == "gateway down"


def test_unknown_body_never_crashes():
    f = ue.classify(418, "<html>teapot</html>")
    assert f.kind in (ue.BAD_REQUEST, ue.UNKNOWN)
    assert f.message


def test_error_payload_keeps_business_code_and_kind():
    f = ue.classify(400, '{"code":11128,"msg":"x"}')
    payload = ue.error_payload(f)
    assert payload["error"]["code"] == 11128
    assert payload["error"]["type"] == ue.BAD_REQUEST
    assert payload["error"]["wb_kind"] == ue.BAD_REQUEST


def test_bearer_tokens_are_redacted():
    f = ue.classify(401, "invalid token Bearer abcdefghijklmnop")
    assert "abcdefghijklmnop" not in f.message
