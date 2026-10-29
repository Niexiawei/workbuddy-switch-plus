"""客户端指纹（CLI / 桌面端）的识别与出站头合成。"""

from core import client_profile


def test_detect_cli_by_ide_type():
    assert client_profile.detect({"X-IDE-Type": "CLI"}) == client_profile.CLI


def test_detect_cli_by_user_agent():
    assert client_profile.detect({"User-Agent": "CLI/2.177.1 CodeBuddy/2.177.1"}) == client_profile.CLI


def test_desktop_ua_is_not_cli():
    # 桌面端 UA 里也有 CLI/ 段，但不在开头，不能误判成 CLI。
    ua = "WorkBuddy/5.5.4 WorkBuddy/5.5.4 CLI/2.137.1"
    assert client_profile.detect({"User-Agent": ua}) == client_profile.DESKTOP
    assert client_profile.detect({}) == client_profile.DESKTOP


def test_merge_passes_cli_fingerprint_through():
    base = {"User-Agent": "codebuddy2openai/2.0", "Authorization": "Bearer acct"}
    incoming = {
        "User-Agent": "CLI/2.177.1 CodeBuddy/2.177.1",
        "X-Product": "SaaS",
        "X-IDE-Type": "CLI",
        "X-IDE-Name": "CLI",
        "X-IDE-Version": "2.177.1",
    }
    out = client_profile.merge_outbound_headers(base, incoming, client_profile.CLI)
    assert out["User-Agent"] == "CLI/2.177.1 CodeBuddy/2.177.1"
    assert out["X-Product"] == "SaaS"
    assert out["X-IDE-Version"] == "2.177.1"
    assert out["Authorization"] == "Bearer acct"  # 鉴权始终用账号的
    # 大小写不敏感覆盖：不能同时留下两个 User-Agent
    assert len([k for k in out if k.lower() == "user-agent"]) == 1


def test_merge_keeps_account_identity_headers():
    base = {"X-User-Id": "uid-1", "X-Enterprise-Id": "ent-1", "X-Domain": "www.codebuddy.cn"}
    incoming = {"X-IDE-Type": "CLI", "X-User-Id": "anonymous", "X-Domain": "evil.example"}
    out = client_profile.merge_outbound_headers(base, incoming, client_profile.CLI)
    assert out["X-User-Id"] == "uid-1"
    assert out["X-Enterprise-Id"] == "ent-1"
    assert out["X-Domain"] == "www.codebuddy.cn"


def test_keep_client_user_id_switch(monkeypatch):
    monkeypatch.setattr(client_profile, "KEEP_CLIENT_USER_ID", True)
    out = client_profile.merge_outbound_headers(
        {"X-User-Id": "uid-1"}, {"X-IDE-Type": "CLI", "X-User-Id": "anonymous"}, client_profile.CLI
    )
    assert out["X-User-Id"] == "anonymous"


def test_desktop_profile_untouched():
    base = {"User-Agent": "codebuddy2openai/2.0"}
    out = client_profile.merge_outbound_headers(base, {"X-IDE-Type": "CLI"}, client_profile.DESKTOP)
    assert out == base


def test_cli_extra_body_keys_are_not_in_default_whitelist():
    from core.converter import PASSTHROUGH_BODY_KEYS

    for key in client_profile.CLI_EXTRA_BODY_KEYS:
        assert key not in PASSTHROUGH_BODY_KEYS, f"{key} 已进默认白名单，CLI_EXTRA_BODY_KEYS 需要收敛"
