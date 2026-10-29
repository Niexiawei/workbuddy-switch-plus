"""API 密钥的生成、校验与限额判定（移植自 workbuddy-manager/server/keysvc.py）。

改为 JSON 存储（store.data["keys"]），不引入 SQLite。密钥条目在原有
`{name, hash, hint, created}` 基础上扩展出策略字段：

  enabled / expires_at / max_ips / ip_allowlist / models / realm /
  quota / used_tokens / quota_credit / used_credit / last_used_at / prefix

拒绝语义（与原实现同口径，按「该怪谁」分流状态码）：
  · 请求与密钥不匹配（版本归属 / 模型白名单 / 缺 model）→ 400 invalid_request_error
  · 凭据本身不可用（已停用 / 已过期）→ 403
  · 用量类（配额用尽）→ 429 insufficient_quota
"""
from __future__ import annotations

import time

from fastapi import HTTPException

from .iputil import ip_matches

DEFAULT_REALM = ""

#: 「CodeBuddy CLI 专用」密钥的用途标记。CLI 需要一个长期有效、可单独吊销的
#: 凭据；把它混在通用密钥里，用户就没法一眼看出哪一枚是给 CLI 的。
CLI_PURPOSE = "cli"


def _hash(token: str) -> str:
    import hashlib
    return hashlib.sha256(token.encode()).hexdigest()


def _reject(status: int, message: str, err_type: str, code: str) -> HTTPException:
    return HTTPException(status_code=status,
                         detail={"error": {"message": message, "type": err_type, "code": code}})


def norm_realm(value: object) -> str:
    """归一键的版本归属；'' = 不限制。"""
    v = str(value or "").strip().lower()
    return v if v in ("cn", "global") else ""


def norm_credit(value: object) -> float:
    """归一化积分额度；0 = 不限，脏值一律吃掉成 0（钳到 0 不会误伤）。"""
    try:
        v = float(value if value not in (None, "") else 0)
    except (TypeError, ValueError):
        return 0.0
    if v != v or v in (float("inf"), float("-inf")):
        return 0.0
    return max(0.0, v)


def norm_cidrs(items: object) -> list[str]:
    """归一化 IP 白名单：逐项去空白、丢弃空项。

    必须在**写入时**归一化：`ip_matches` 对带空格的 CIDR 解析失败 →
    这条规则永远匹配不上 → 白名单里只要有一条这样的记录，密钥就对所有来源拒绝。
    """
    if not isinstance(items, list):
        return []
    out: list[str] = []
    for raw in items:
        s = str(raw).strip()
        if s:
            out.append(s)
    return out


def _norm_groups(store, values) -> list[str]:
    """归一化密钥的分组绑定（丢掉不存在的 gid）。

    复用 admin/groups.py 的实现而不是各写一份：分组是否存在的判据只能有一处，
    否则「创建时校验、更新时不校验」这类不对称迟早会出现。

    只读 `store.data`，**不加锁**——调用方（create_key / update_key）已经持锁，
    threading.Lock 不可重入，这里再加一次就是死锁。
    """
    from .groups import norm_groups
    return norm_groups(store, values)


def _bare_model(model: object) -> str:
    """模型名归一化：去 `cn:` 前缀（`global:` 保留），用于白名单比对。"""
    m = str(model or "").strip()
    return m[3:] if m.lower().startswith("cn:") else m


def _model_realm(model: object) -> str:
    return "global" if str(model or "").strip().lower().startswith("global:") else "cn"


def model_allowed(key: dict, model: object) -> bool:
    """该密钥的模型白名单是否放行这个模型；白名单为空 = 不限制。"""
    allow = key.get("models") or []
    if not allow:
        return True
    if not isinstance(model, str) or not model.strip():
        return False
    return _bare_model(model) in {_bare_model(x) for x in allow}


def public_key(kid: str, item: dict) -> dict:
    """管理端可见的密钥视图（绝不包含 hash）。

    `secret` 是明文密钥：本服务定位是单机自用（state.json 本来就在用户自己
    的磁盘上，账号凭据也在同一份文件里），所以允许管理端回显明文，便于
    随时复制到客户端。前端默认显示、可一键隐藏。
    """
    return {
        "id": kid,
        "name": item.get("name", ""),
        "hint": item.get("hint", ""),
        "prefix": item.get("prefix", ""),
        "secret": item.get("secret", ""),
        "enabled": bool(item.get("enabled", True)),
        "expires_at": item.get("expires_at"),
        "max_ips": item.get("max_ips", 0),
        "ip_allowlist": list(item.get("ip_allowlist") or []),
        "models": list(item.get("models") or []),
        "realm": norm_realm(item.get("realm")),
        "groups": list(item.get("groups") or []),
        "quota": item.get("quota", 0),
        "used_tokens": item.get("used_tokens", 0),
        "quota_credit": item.get("quota_credit", 0),
        "used_credit": item.get("used_credit", 0),
        "created": item.get("created"),
        "last_used_at": item.get("last_used_at"),
        "ips": list(item.get("ips") or []),
        "purpose": norm_purpose(item.get("purpose")),
    }


def list_keys(store, *, reveal_secret: bool = False) -> list[dict]:
    """密钥列表。

    ## `reveal_secret` 为什么必须显式传

    原先这里**无条件**返回 `public_key()`，而它带 `secret` = 明文密钥。
    调用方 `/admin/api/overview` 只要求 `viewer`（比 admin 弱），
    于是任何持 readonly 身份的人——包括一枚最低权限的 `wbt_` 令牌——
    都能拿到全站所有密钥的明文，拿去就能直接调用上游，
    等于一次性拿走所有人的额度。已用真实 readonly 令牌 PoC 坐实。

    所以默认值改成 `False`（不回显），要让调用方**明确说**"这里可以给明文"。
    默认安全、显式放开：将来新增调用点时忘传参数，后果是"看不到密钥"
    （用户会立刻反馈），而不是"悄悄泄露全站密钥"（没人会发现）。

    ⚠️ `secret` 字段本身**始终保留**（admin 拿到的仍是完整视图），
       不改成删除键是为了不破坏前端 —— 它读 `k.secret` 渲染"复制"按钮，
       键没了会变成 `undefined` 而不是空串。只读身份拿到的是空串。
    """
    with store.lock:
        rows = list(store.data["keys"].items())
        ips = store.data.get("api_key_ips", {})
        out = []
        for kid, item in rows:
            view = public_key(kid, item)
            if not reveal_secret:
                # 只抹掉明文本身。`hint` / `prefix` 保留 ——
                # 它们本来就是设计给人看的（sk-wb-abc…），不含足够猜出全文的信息，
                # 而列表页要靠它们区分"是哪一枚"。
                view["secret"] = ""
            view["ips"] = list(ips.get(kid, []))
            out.append(view)
    out.sort(key=lambda k: k.get("created") or 0, reverse=True)
    return out


def norm_purpose(value: object) -> str:
    """密钥用途：目前只认 `cli`（CodeBuddy CLI 专用），其余一律当通用密钥。"""
    text = str(value or "").strip().lower()
    return CLI_PURPOSE if text == CLI_PURPOSE else ""


def create_key(store, name, *, value=None, expires_at=None, max_ips=0,
               ip_allowlist=None, models=None, realm="", quota=0,
               quota_credit=0, enabled=True, created_by="", groups=None,
               purpose="",
               ) -> tuple[str, str]:
    import secrets
    token = value or "sk-wb-" + secrets.token_urlsafe(32)
    kid = secrets.token_hex(8)
    entry = {
        "name": str(name),
        "hash": _hash(token),
        # 明文：见 public_key() 的说明。老条目没有这个字段，回显会是空串。
        "secret": token,
        "hint": token[:5] + "…" + token[-4:],
        "prefix": token[:12],
        "enabled": bool(enabled),
        "expires_at": int(expires_at) if expires_at else None,
        "max_ips": int(max_ips or 0),
        "ip_allowlist": norm_cidrs(ip_allowlist),
        "models": [str(m).strip() for m in (models or []) if str(m).strip()],
        "realm": norm_realm(realm),
        "groups": _norm_groups(store, groups),
        "quota": int(quota or 0),
        "used_tokens": 0,
        "quota_credit": norm_credit(quota_credit),
        "used_credit": 0.0,
        "created": int(time.time()),
        "last_used_at": None,
        "created_by": str(created_by or "")[:64],
        "purpose": norm_purpose(purpose),
    }
    with store.lock:
        if len(store.data["keys"]) >= 100:
            raise HTTPException(400, "最多保存 100 枚 API Key")
        store.data["keys"][kid] = entry
        store.save()
    return kid, token


def find_cli_key(store) -> dict | None:
    """当前可用的 CLI 专用密钥（启用且未过期），没有就 None。"""
    now = time.time()
    with store.lock:
        for kid, item in store.data["keys"].items():
            if norm_purpose(item.get("purpose")) != CLI_PURPOSE:
                continue
            if not item.get("enabled", True):
                continue
            if item.get("expires_at") and int(item["expires_at"]) <= now:
                continue
            return {"id": kid, **item}
    return None


def ensure_cli_key(store, *, created_by="") -> tuple[str, str, bool]:
    """保证存在一枚 CLI 专用密钥，返回 (id, 明文, 是否新建)。

    realm 固定 `cn`：国内版 CLI 只该打到国内账号，混用会让国际版账号被 400 拦下。
    `max_ips` 不设限：本机 127.0.0.1 与局域网自测都可能出现，硬卡一个 IP
    只会让「换台机器试一下」直接失败。
    """
    existing = find_cli_key(store)
    if existing:
        return existing["id"], str(existing.get("secret") or ""), False
    kid, token = create_key(store, "CodeBuddy CLI", realm="cn",
                            purpose=CLI_PURPOSE, created_by=created_by)
    return kid, token, True


def revoke_cli_keys(store) -> int:
    """吊销所有 CLI 专用密钥；返回吊销数量。

    与 `delete_key` 一样保留「不能删掉最后一枚密钥」的保护，否则会把
    数据面锁死。
    """
    with store.lock:
        ids = [kid for kid, item in store.data["keys"].items()
               if norm_purpose(item.get("purpose")) == CLI_PURPOSE]
        if not ids:
            return 0
        if len(store.data["keys"]) - len(ids) <= 0:
            raise HTTPException(400, "CLI 密钥是当前唯一密钥，请先创建其它密钥再吊销")
        for kid in ids:
            store.data["keys"].pop(kid, None)
            (store.data.get("api_key_ips") or {}).pop(kid, None)
        store.save()
    return len(ids)


def rotate_cli_key(store, *, created_by="") -> tuple[str, str]:
    """换一枚新的 CLI 密钥（旧的立即失效）。

    先建新、再删旧：反过来的话「CLI 密钥是唯一一枚」时会被 delete 的保护拦住，
    而轮换本身不会出现没有密钥的瞬间。
    """
    kid, token = create_key(store, "CodeBuddy CLI", realm="cn",
                            purpose=CLI_PURPOSE, created_by=created_by)
    with store.lock:
        for old, item in list(store.data["keys"].items()):
            if old == kid or norm_purpose(item.get("purpose")) != CLI_PURPOSE:
                continue
            store.data["keys"].pop(old, None)
            (store.data.get("api_key_ips") or {}).pop(old, None)
        store.save()
    return kid, token


def update_key(store, kid: str, patch: dict) -> dict:
    with store.lock:
        item = store.data["keys"].get(kid)
        if not item:
            raise HTTPException(404, "密钥不存在")
        if "name" in patch and patch["name"] is not None:
            if not str(patch["name"]).strip():
                raise HTTPException(400, "密钥名称不能只有空格")
            item["name"] = str(patch["name"]).strip()[:60]
        if "enabled" in patch:
            item["enabled"] = bool(patch["enabled"])
        if "expires_at" in patch:
            v = patch["expires_at"]
            item["expires_at"] = int(v) if v else None
        if "max_ips" in patch:
            item["max_ips"] = int(patch["max_ips"] or 0)
        if "ip_allowlist" in patch:
            item["ip_allowlist"] = norm_cidrs(patch["ip_allowlist"])
        if "models" in patch:
            item["models"] = [str(m).strip() for m in (patch["models"] or []) if str(m).strip()]
        if "realm" in patch:
            item["realm"] = norm_realm(patch["realm"])
        if "groups" in patch:
            item["groups"] = _norm_groups(store, patch["groups"])
        if "quota" in patch:
            item["quota"] = int(patch["quota"] or 0)
        if "quota_credit" in patch:
            item["quota_credit"] = norm_credit(patch["quota_credit"])
        store.save()
        return public_key(kid, item)


def delete_key(store, kid: str) -> dict:
    """删除一枚密钥。

    ⚠️ 不允许删掉实例里的最后一枚密钥：那会让管理员把自己锁在门外。
    """
    with store.lock:
        if kid not in store.data["keys"]:
            raise HTTPException(404, "密钥不存在")
        if len(store.data["keys"]) <= 1:
            raise HTTPException(400, "请先创建一枚新密钥，再撤销最后一枚密钥")
        del store.data["keys"][kid]
        store.data.get("api_key_ips", {}).pop(kid, None)
        store.save()
    return {"ok": True}


def reset_usage(store, kid: str) -> dict:
    """把已用 token 与已用积分一起归零。"""
    with store.lock:
        item = store.data["keys"].get(kid)
        if not item:
            raise HTTPException(404, "密钥不存在")
        item["used_tokens"] = 0
        item["used_credit"] = 0.0
        store.save()
    return {"ok": True}


def resolve_key(store, token: str) -> dict | None:
    """按哈希比对定位密钥（n ≤ 100，逐条常量时间比较即可）。"""
    if not token:
        return None
    hashed = _hash(token)
    import hmac
    with store.lock:
        for kid, item in store.data["keys"].items():
            if hmac.compare_digest(item.get("hash", ""), hashed):
                return {"id": kid, **item}
    return None


def _num(value) -> float:
    """宽容地把一个可能脏掉的值读成数字；读不出来就当 0。

    用于 validate_credential：那几个额度/有效期字段来自人工可编辑的
    state.json，脏值应该表现为「这个限制不生效」，而不是让整条请求 500。
    """
    if isinstance(value, bool):
        return 0.0
    if isinstance(value, (int, float)):
        # NaN 参与比较永远是 False，会让额度判断静默失效，所以归零。
        return 0.0 if value != value else float(value)
    try:
        return float(str(value).strip())
    except (TypeError, ValueError):
        return 0.0

def validate_credential(key: dict) -> None:
    """只校验**凭据本身**（启停 / 有效期 / 额度）；不涉及来源 IP 与请求模型。

    供数据面的「先鉴权后读 body」阶段使用：那时还拿不到 model，不能拿它去判
    模型白名单（否则带白名单的密钥会被误判成 model_not_allowed）。
    """
    if not key.get("enabled", True):
        raise _reject(403, "密钥已停用", "permission_error", "key_disabled")
    # 下面几个字段必须走 _num()/_int() 转换，不能直接 int()/float()。
    #
    # 这些值存在 state.json 里，那个文件是人工可编辑的，也会被老版本写过。
    # 只要有一个字段脏了（{"quota": "unlimited"}、{"expires_at": "never"}），
    # 直接 int() 就会抛 ValueError —— 那不是干净的 4xx，而是未捕获异常：
    # 拿着这枚密钥的每一个请求都会 500，包括本来该正常放行的模型发现请求。
    # 用户看到的是「服务坏了」，而不是「这枚密钥有问题」。
    #
    # 这个模块里其它归一化函数（norm_credit / norm_realm / norm_cidrs）
    # 都是刻意宽容的，只有这里漏了 —— 统一成同样的宽容策略。
    exp = _num(key.get("expires_at"))
    if exp and exp < time.time():
        raise _reject(403, "密钥已过期", "permission_error", "key_expired")
    quota = _num(key.get("quota"))
    if quota and _num(key.get("used_tokens")) >= quota:
        raise _reject(429, "密钥配额已用尽", "insufficient_quota", "quota_exhausted")
    qc = _num(key.get("quota_credit"))
    used = _num(key.get("used_credit"))
    if qc and used >= qc:
        raise _reject(
            429,
            f"密钥积分额度已用尽（已用 {used:g} / 上限 {qc:g}）",
            "insufficient_quota", "credit_quota_exhausted")


def validate(key: dict, ip: str | None, model: str | None, *,
             is_model_list: bool = False, mapped_model: str | None = None) -> None:
    """通过则返回 None，否则抛 HTTPException。

    is_model_list：请求是模型发现（无 model），版本归属与模型白名单不拦。
    mapped_model：model 经模型映射后的名字；版本归属判映射后的名字。
    """
    validate_credential(key)

    allow = key.get("ip_allowlist") or []
    if ip and allow and not any(ip_matches(ip, c) for c in allow):
        raise _reject(400, f"来源 IP {ip} 不在密钥白名单内",
                      "invalid_request_error", "ip_not_allowed")

    max_ips = int(key.get("max_ips") or 0)
    if max_ips and ip:
        seen = list(key.get("ips") or [])
        if ip not in seen and len(seen) >= max_ips:
            raise _reject(400, f"密钥已绑定 {len(seen)} 个 IP，超出上限 {max_ips}",
                          "invalid_request_error", "too_many_ips")

    effective = mapped_model or model
    want = norm_realm(key.get("realm"))
    if want and not is_model_list:
        if not isinstance(effective, str) or not effective.strip():
            label = "国际版" if want == "global" else "国内版"
            raise _reject(400, f"该密钥限定了{label}模型，请求必须指定 model",
                          "invalid_request_error", "realm_mismatch")
        got = _model_realm(effective)
        if got != want:
            label = {"cn": "国内版", "global": "国际版"}[want]
            other = "国际版" if want == "cn" else "国内版"
            hint = ("模型名需带 global: 前缀" if want == "global" else "请去掉 global: 前缀")
            raise _reject(400, f"该密钥仅限{label}模型，当前请求是{other}模型（{hint}）",
                          "invalid_request_error", "realm_mismatch")

    if key.get("models") and not is_model_list:
        if not isinstance(model, str) or not model.strip():
            raise _reject(400, "请求未指定 model，而该密钥启用了模型白名单",
                          "invalid_request_error", "model_not_allowed")
        if not model_allowed(key, model):
            raise _reject(400, f"模型 {model} 不在密钥白名单内",
                          "invalid_request_error", "model_not_allowed")


def touch_key(store, kid: str, ip: str | None, tokens: int = 0, credit: float | None = None) -> None:
    """记账：来源 IP、最近使用、已用 token 与已用积分（旁路，异常不影响请求）。"""
    try:
        with store.lock:
            item = store.data["keys"].get(kid)
            if not item:
                return
            if ip:
                ips = store.data.setdefault("api_key_ips", {}).setdefault(kid, [])
                if ip not in ips and len(ips) < 64:
                    ips.append(ip)
            item["last_used_at"] = int(time.time())
            if tokens:
                item["used_tokens"] = int(item.get("used_tokens", 0)) + int(tokens)
            if credit:
                item["used_credit"] = round(float(item.get("used_credit", 0.0)) + float(credit), 6)
            store.save()
    except Exception:  # noqa: BLE001
        pass


def model_aliases(store) -> dict[str, str]:
    """读模型别名表（`store.data["model_aliases"]`）：别名 → 真实模型名。

    下游客户端的模型名往往写死在配置里，换模型就得挨个改。有了这一层，
    把「旧名字 → 新模型」写进别名表，客户端一个字都不用动。
    """
    with store.lock:
        raw = store.data.get("model_aliases") or {}
    out: dict[str, str] = {}
    if isinstance(raw, dict):
        for key, value in raw.items():
            name, target = str(key).strip(), str(value).strip()
            if name and target and name != target:
                out[name] = target
    return out


def resolve_model_alias(store, model: object) -> str:
    """把客户端传来的别名解析成真实模型名；不是别名就原样返回。

    支持链式（a → b → c）但**防环**：环上绕回起点就停下，返回绕回来的那个名字，
    否则配置里手滑写了个环会让数据面直接死循环。
    """
    name = str(model or "").strip()
    if not name:
        return name
    table = model_aliases(store)
    if not table:
        return name
    seen = {name}
    while name in table:
        nxt = table[name]
        if nxt in seen:
            break
        seen.add(nxt)
        name = nxt
    return name


def unknown_whitelist_entries(models: list[str], known: set[str] | None,
                              aliases: set[str] | None = None) -> list[str]:
    """白名单里匹配不到任何已知模型的条目（多半是拼错了）。

    `known` 为 None（清单拿不到）时**不判**——假警报比不提示更糟。
    """
    if not models or known is None:
        return []
    alias_bare = {str(x).strip() for x in (aliases or set())}
    known_bare = {_bare_model(x) for x in known}
    out: list[str] = []
    for raw in models:
        entry = str(raw).strip()
        if not entry:
            continue
        if entry in alias_bare or _bare_model(entry) in alias_bare:
            continue
        if _bare_model(entry) in known_bare:
            continue
        out.append(entry)
    return out
