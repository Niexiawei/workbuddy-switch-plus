"""客户端指纹：区分 CodeBuddy CLI 与桌面端，并按 profile 组装上游请求。

为什么需要这一层：CLI 用 `CODEBUDDY_BASE_URL` 直连本反代时，请求形状与桌面端
不同（UA 带 CLI 版本段、多 `X-Product` / `X-IDE-*`，body 带 `extra_vars` /
`cache_retention`）。反代若一律按桌面端指纹改写，上游看到的会是「既不是 CLI，
也不是桌面端」的混合流量；字段白名单还会把 CLI 的新字段静默丢掉。

判定只看客户端自己发的头，不看路径——两个 profile 共用同一批 `/v1/*` 路由。
"""

from __future__ import annotations

CLI = "cli"
DESKTOP = "desktop"

#: 上游按客户端原样接收的头（小写）。UA 的版本段随安装版本变（2.177.1 / 2.137.1 …），
#: 反代不能硬编码，否则每次 CLI 升级都会变成「旧版本客户端」。
CLI_HEADER_PASSTHROUGH = (
    "user-agent",
    "x-product",
    "x-ide-type",
    "x-ide-name",
    "x-ide-version",
    "x-requested-with",
    "x-codebuddy-request",
)

#: CLI 独有、必须透传的请求体字段（不在默认白名单里）。
#: `extra_vars` 带 harnessMetadata（含 harnessVersion / stepIndex），
#: `cache_retention` 决定上游缓存保留策略——丢掉它们等于改变了请求语义。
CLI_EXTRA_BODY_KEYS = ("extra_vars", "cache_retention")

#: `X-User-Id` 发账号 uid 还是客户端原值（CLI 发的是 `anonymous`）。
#:
#: 默认发账号 uid：上游的配额与风控按它归属，发 anonymous 会让额度落进匿名桶。
#: 这是实测待定项（计划文档 G3），需要时改这里做 A/B。
KEEP_CLIENT_USER_ID = False

#: 反代必须保留、不允许客户端覆盖的头（身份与鉴权以账号为准）。
_RESERVED = ("authorization", "content-type", "accept", "x-user-id", "x-enterprise-id",
             "x-tenant-id", "x-domain")


def lower_headers(headers) -> dict:
    """把任意请求头映射归一成小写键；`None` 视为空。

    Starlette 的 `Headers`、普通 dict、`dict(scope["headers"])` 的字节形态都吃得下，
    这样调用方不必先转换。
    """
    out: dict[str, str] = {}
    if not headers:
        return out
    items = headers.items() if hasattr(headers, "items") else headers
    for key, value in items:
        if isinstance(key, bytes):
            key = key.decode("latin-1", "replace")
        if isinstance(value, bytes):
            value = value.decode("latin-1", "replace")
        out[str(key).strip().lower()] = str(value)
    return out


def detect(headers) -> str:
    """判定请求来自 CLI 还是桌面端。

    判据只认两种客户端都会明确发送的信号：
      · `X-IDE-Type: CLI`（CLI 2.x 必发）；
      · UA 以 `CLI/` 开头（同上，作为头被中间层改写时的兜底）。
    桌面端的 UA 是 `WorkBuddy/5.5.4 … CLI/2.137.1`，虽然含 `CLI/` 但不在开头，
    所以不会被误判。
    """
    h = lower_headers(headers)
    if h.get("x-ide-type", "").strip().lower() == CLI:
        return CLI
    ua = h.get("user-agent", "").strip().lower()
    if ua.startswith("cli/"):
        return CLI
    return DESKTOP


def merge_outbound_headers(base: dict, request_headers, profile: str = DESKTOP) -> dict:
    """按 profile 合成真正发往上游的请求头。

    CLI 流量把客户端自己的指纹头原样带上；鉴权与身份头（Authorization /
    X-User-Id / X-Enterprise-Id / X-Tenant-Id / X-Domain）始终以所选账号为准，
    不允许客户端覆盖——这几个字段决定「用哪个号、算谁的额度」。
    """
    merged = dict(base)
    if profile != CLI:
        return merged
    incoming = lower_headers(request_headers)
    for name in CLI_HEADER_PASSTHROUGH:
        if name in _RESERVED:
            continue
        value = incoming.get(name)
        if value:
            _set_header(merged, name, value)
    if KEEP_CLIENT_USER_ID and incoming.get("x-user-id"):
        _set_header(merged, "x-user-id", incoming["x-user-id"])
    return merged


def _set_header(target: dict, name: str, value: str) -> None:
    """大小写不敏感地覆盖一个头。

    直接赋值不够：账号头里已经有 `User-Agent`，再写一个 `user-agent` 会变成
    两个同名头，httpx 发出去后上游拿到的可能是旧的那个（桌面端 UA），
    CLI 指纹就白透了。
    """
    for existing in list(target):
        if existing.lower() == name:
            del target[existing]
    target[_CANONICAL.get(name, name)] = value


_CANONICAL = {
    "user-agent": "User-Agent",
    "x-product": "X-Product",
    "x-ide-type": "X-IDE-Type",
    "x-ide-name": "X-IDE-Name",
    "x-ide-version": "X-IDE-Version",
    "x-requested-with": "x-requested-with",
    "x-codebuddy-request": "x-codebuddy-request",
    "x-user-id": "X-User-Id",
}
