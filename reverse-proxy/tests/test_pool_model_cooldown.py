"""G6：业务码分类 → 账号级 / 模型级冷却，且只影响该模型。"""

import json

import pytest
from fastapi import HTTPException

from admin.pool import AccountPool, DEFAULT_POOL
from admin.switch_store import SwitchStore
from core import upstream_errors as ue


ACCOUNT = {
    "id": "probe1", "uid": "uid-probe", "nickname": "probe",
    "access_token": "fake-token", "expiresAt": 4102444800000,
    "domain": "www.codebuddy.cn", "variant": "cn",
}


def make_pool(tmp_path):
    accounts = tmp_path / "accounts.json"
    accounts.write_text(json.dumps([ACCOUNT], ensure_ascii=False), encoding="utf-8")
    store = SwitchStore(root=str(tmp_path / "state"), accounts_path=accounts,
                        admin_key="a" * 40)
    store.data["pool"] = {**DEFAULT_POOL, "auto_checkin": False, "auto_travel": False,
                          "auto_keepalive": False, "auto_active": False}
    store.save()
    return AccountPool(store), list(store.data["accounts"])[0], store


def test_rate_limit_cools_only_that_model(tmp_path):
    pool, aid, store = make_pool(tmp_path)
    failure = ue.classify(400, '{"code":11134,"msg":"rate limited"}')
    pool.record(aid, 429, model="glm-5.3", failure=failure)

    assert pool.model_cooling(aid, "glm-5.3")
    assert not pool.model_cooling(aid, "deepseek-v4-pro")
    with pytest.raises(HTTPException) as exc:
        pool.select("aff", model="glm-5.3")
    assert exc.value.status_code == 503
    assert pool.select("aff", model="deepseek-v4-pro")[0] == aid
    store.stop_runtime_writer()


def test_channel_check_cools_nothing(tmp_path):
    pool, aid, store = make_pool(tmp_path)
    failure = ue.classify(400, '{"code":11128,"msg":"channel check"}')
    pool.record(aid, 400, model="glm-5.3", failure=failure)

    assert not pool.model_cooling(aid, "glm-5.3")
    assert store.data["account_status"][aid].get("cooldown_until", 0) == 0
    assert pool.select("aff", model="glm-5.3")[0] == aid
    store.stop_runtime_writer()


def test_auth_failure_cools_whole_account(tmp_path):
    pool, aid, store = make_pool(tmp_path)
    failure = ue.classify(401, "")
    pool.record(aid, 401, model="glm-5.3", failure=failure)

    assert store.data["account_status"][aid]["cooldown_until"] > 0
    with pytest.raises(HTTPException):
        pool.select("aff", model="glm-5.3")
    store.stop_runtime_writer()
