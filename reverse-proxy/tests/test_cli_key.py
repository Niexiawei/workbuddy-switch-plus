"""G8：CLI 专用密钥的签发 / 轮换 / 吊销。"""

import json

import pytest
from fastapi import HTTPException

from admin import keysvc
from admin.switch_store import SwitchStore


@pytest.fixture()
def store(tmp_path):
    accounts = tmp_path / "accounts.json"
    accounts.write_text("[]", encoding="utf-8")
    s = SwitchStore(root=str(tmp_path / "state"), accounts_path=accounts,
                    admin_key="a" * 40)
    yield s
    s.stop_runtime_writer()


def test_ensure_is_idempotent_and_realm_bound(store):
    kid1, token1, created1 = keysvc.ensure_cli_key(store)
    kid2, token2, created2 = keysvc.ensure_cli_key(store)
    assert created1 is True and created2 is False
    assert (kid1, token1) == (kid2, token2)

    key = keysvc.resolve_key(store, token1)
    assert key["purpose"] == "cli"
    assert key["realm"] == "cn"
    assert key["name"] == "CodeBuddy CLI"


def test_ensure_ignores_normal_keys(store):
    normal, _ = keysvc.create_key(store, "普通密钥", realm="cn")
    kid, token, created = keysvc.ensure_cli_key(store)
    assert created is True and kid != normal


def test_rotate_invalidates_old_secret(store):
    _, old_token, _ = keysvc.ensure_cli_key(store)
    new_kid, new_token = keysvc.rotate_cli_key(store)
    assert new_token != old_token
    assert keysvc.resolve_key(store, old_token) is None
    assert keysvc.resolve_key(store, new_token)["id"] == new_kid


def test_revoke_removes_cli_keys(store):
    _, token, _ = keysvc.ensure_cli_key(store)
    keysvc.create_key(store, "保底密钥")
    assert keysvc.revoke_cli_keys(store) == 1
    assert keysvc.resolve_key(store, token) is None
    assert keysvc.ensure_cli_key(store)[2] is True   # 可以再签一枚


def test_revoke_refuses_to_remove_last_key(store):
    keysvc.ensure_cli_key(store)                      # 这是唯一一枚
    with pytest.raises(HTTPException) as exc:
        keysvc.revoke_cli_keys(store)
    assert exc.value.status_code == 400


def test_purpose_not_settable_to_arbitrary_value(store):
    _, token = keysvc.create_key(store, "x", purpose="../../etc")
    assert keysvc.resolve_key(store, token)["purpose"] == ""
