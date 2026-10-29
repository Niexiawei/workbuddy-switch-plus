"""Small, persistent management layer for workbuddy2api. Single-process deployment."""
import asyncio
import ipaddress
import json
import math
import os
import secrets
import sys
import threading
import hmac
import time
from collections import deque
from pathlib import Path
from contextlib import asynccontextmanager
from urllib.parse import urlparse

import httpx
import uvicorn
from fastapi import FastAPI, HTTPException, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse, RedirectResponse

from core import converter
from . import alerts
from . import assets
from . import egress
from . import groups
from . import keysvc
from . import mirror
from . import realm as realm_mod
from . import settings_store
from . import throttle
from . import version as _version
from . import keyexport
from . import usage
from .browser_login import BrowserLogin
from . import browser_login as browser_login_mod
from .pool import TASK_KINDS, AccountPool, PoolMiddleware, default_pool
from .metrics import RequestMetrics, MetricsMiddleware
from .security import COOKIE, Security, bearer_session, digest, scope_to_role
from . import security as security_mod
from .iputil import client_ip, client_ip_from_scope, evaluate, valid_cidr

MAX_BODY = 1024 * 1024

# 静态资源目录。**经 `assets.get()` 解析**，不再是裸的 `_MEIPASS`。
#
# 原先这里是 `Path(_MEIPASS) / "admin" / "static"` —— 在服务器上会炸：
# 单文件 exe 把资源解包到 `%TEMP%\__MEIxxxxx`，而磁盘清理会删掉它，
# 一删所有页面立刻 404（API 却还通，日志也不报错，只有网页全挂）。
# 现在改为首次启动固化到 exe 同级，详见 `admin/assets.py` 的长注释。
ASSETS = assets.get()
IP_LOG_LIMIT = 500

# 自助密钥的数量上限（历史遗留常量）。
#
# 为什么不直接用 keysvc 的全局上限（100）：那是**全实例**的总量，
# 一个人建满 100 枚就能把所有人的额度吃光，其他人一枚都建不了。
# 每人限一个小额度，全局上限仍然是最后一道兜底。
ME_KEY_LIMIT = 20


def write_json(path, value):
    tmp = path.with_suffix(path.suffix + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as out:
        json.dump(value, out, ensure_ascii=False, indent=2)
        out.flush()
        os.fsync(out.fileno())
    os.replace(tmp, path)


def clean_name(value, fallback):
    if value is None:
        return fallback
    if not isinstance(value, str) or not 1 <= len(value.strip()) <= 60:
        raise HTTPException(400, "名称需为 1–60 个字符")
    return value.strip()


def live_accounts(store) -> None:
    """让账号视图对齐账号库（SwitchStore 支持热加载，旧 Store 无此概念）。

    代价只有一次 stat()：指纹没变就直接返回，所以在每个涉及账号的
    请求前调用是安全的。
    """
    refresh = getattr(store, "refresh_accounts", None)
    if refresh is not None:
        refresh()


class Store:
    def __init__(self, root, auth_dir, initial_key, admin_key):
        if len(admin_key) < 20:
            raise RuntimeError("ADMIN_KEY must contain at least 20 characters")
        self.root, self.auth_dir = Path(root), Path(auth_dir)
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.auth_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.lock = threading.RLock()
        self.admin_digest = digest(admin_key)
        self.admin_key = admin_key
        self.sessions, self.attempts = {}, {}
        self.test_lock = asyncio.Lock()
        self.test_keys = set()
        self.managers = {}
        self.started = time.time()
        self.events = deque(maxlen=40)
        self.path = self.root / "state.json"
        if self.path.exists():
            self.data = json.loads(self.path.read_text(encoding="utf-8"))
        else:
            self.data = {"accounts": {}, "active": None, "keys": {}, "pool": default_pool()}
            for p in sorted(self.auth_dir.glob("*.info")):
                try:
                    doc = json.loads(p.read_text(encoding="utf-8"))
                    self.validate_credential(doc)
                except (ValueError, OSError, HTTPException):
                    continue
                aid = secrets.token_hex(8)
                name = str(doc.get("account", {}).get("nickname") or "已迁移账号")[:60]
                self.data["accounts"][aid] = {"name": name, "file": p.name, "enabled": not bool(doc.get("disabled")), "created": int(time.time())}
                if self.data["active"] is None and not doc.get("disabled"):
                    self.data["active"] = aid
            if initial_key:
                self.add_key("原有 API Key", initial_key)
            self.save()
        self.sync_active()

    @staticmethod
    def validate_credential(doc):
        if not isinstance(doc, dict) or not isinstance(doc.get("auth"), dict) or not isinstance(doc.get("account"), dict):
            raise HTTPException(400, "需要桌面端 .info／JSON 登录文件，包含 auth 和 account 对象")
        for key in ("accessToken", "refreshToken"):
            if not isinstance(doc["auth"].get(key), str) or not doc["auth"][key].strip():
                raise HTTPException(400, "凭据缺少 accessToken 或 refreshToken，请重新导出桌面端登录文件")
            if len(doc["auth"][key]) > 65536:
                raise HTTPException(400, "凭据字段过长")
        if not isinstance(doc["account"].get("uid"), str) or not doc["account"]["uid"]:
            raise HTTPException(400, "凭据缺少 account.uid")
        if not isinstance(doc["auth"].get("expiresAt"), (int, float)) or isinstance(doc["auth"].get("expiresAt"), bool):
            raise HTTPException(400, "凭据需要有效的 expiresAt 时间戳")
        for key in ("domain",):
            value = doc["auth"].get(key, "")
            if not isinstance(value, str) or "\r" in value or "\n" in value:
                raise HTTPException(400, "凭据包含无效字段")
        for key in ("uid", "enterpriseId"):
            value = doc["account"].get(key, "")
            if not isinstance(value, str) or "\r" in value or "\n" in value:
                raise HTTPException(400, "账号字段格式不正确")
        # realm 允许缺省：存量 .info（本服务升级前导出，或旧版桌面端导出）
        # 里没有这个字段，靠 domain 兜底判归属。**不要在这里填默认值**，
        # 否则会把“未声明”和“声明为国内版”混为一谈，写入时反而多出字段。
        realm = doc["auth"].get("realm")
        if realm is not None:
            if not isinstance(realm, str) or "\r" in realm or "\n" in realm:
                raise HTTPException(400, "凭据包含无效字段")
            if realm_mod.norm_realm(realm) != realm.strip().lower():
                raise HTTPException(400, "凭据 realm 取值无效（只接受 cn / global）")

    def save(self):
        write_json(self.path, self.data)

    def save_browser_account(self, doc, name):
        self.validate_credential(doc)
        with self.lock:
            # 身份三元组，不是二元组。
            #
            # 只按 (uid, enterpriseId) 匹配会出错：同一个 uid 在国内版与国际版
            # 是**两个不同的账号**（额度、套餐、trial 都各自独立），而国际版账号
            # 的 enterpriseId 语义与国内版并不对齐（有的为空、有的给租户号）。
            # 结果就是重新登录一个已有的国际服账号时匹配不上，新建一条 id，
            # 旧记录仍占着 active —— 界面上表现为「切换后显示未选中账号」，
            # 而且旧记录的 token 可能还是有效的，池里于是出现两条同 uid 记录，
            # 轮询到哪条全看顺序。
            identity = (doc["account"]["uid"], doc["account"].get("enterpriseId", ""),
                        realm_mod.realm_of(doc.get("auth") or {}))
            for aid, item in self.data["accounts"].items():
                try:
                    old = json.loads(self.file_for(item).read_text(encoding="utf-8"))
                except (ValueError, OSError):
                    continue
                if (old.get("account", {}).get("uid"), old.get("account", {}).get("enterpriseId", ""),
                        realm_mod.realm_of(old.get("auth") or {})) == identity:
                    current = self.manager_for(aid, item)
                    if current:
                        with current._lock:
                            write_json(self.file_for(item), doc)
                            current._cached = None
                    else:
                        write_json(self.file_for(item), doc)
                    if name:
                        item["name"] = name
                    self.data.get("account_status", {}).pop(aid, None)
                    self.save()
                    self.sync_active()
                    return {"account_id": aid, "updated": True}
            if len(self.data["accounts"]) >= 100:
                raise HTTPException(400, "最多保存 100 个账号")
            aid = secrets.token_hex(8)
            write_json(self.auth_dir / (aid + ".info"), doc)
            self.data["accounts"][aid] = {"file": aid + ".info", "name": name or str(doc["account"].get("nickname") or "浏览器授权账号")[:60], "created": int(time.time()), "enabled": True}
            if self.data["active"] is None:
                self.data["active"] = aid
            self.save()
            self.sync_active()
            return {"account_id": aid, "updated": False}

    def file_for(self, item):
        path = self.auth_dir / item["file"]
        if path.parent.resolve() != self.auth_dir.resolve() or path.is_symlink():
            raise HTTPException(400, "凭据路径不合法")
        return path

    def sync_active(self):
        aid = self.data.get("active")
        item = self.data["accounts"].get(aid)
        converter.CONFIG["cred"] = self.manager_for(aid, item) if item and item["enabled"] and self.file_for(item).exists() else None

    def manager_for(self, aid, item):
        with self.lock:
            if aid not in self.managers:
                self.managers[aid] = converter.CredentialManager(self.file_for(item))
            return self.managers[aid]

    def account_rows(self):
        result = []
        for aid, item in self.data["accounts"].items():
            row = {"id": aid, "name": item["name"], "enabled": item["enabled"], "active": aid == self.data["active"], "created": item["created"]}
            try:
                doc = json.loads(self.file_for(item).read_text(encoding="utf-8"))
                expiry = doc["auth"].get("expiresAt", 0)
                account = doc["account"]
                # 企业版标记：企业账号的配额不走个人版资源接口，界面要能看出来。
                # `enterpriseId` 由浏览器登录链路与导入链路写入，个人账号为空。
                enterprise_id = str(account.get("enterpriseId") or account.get("enterprise_id") or "")
                # 归属：显式 realm 优先，缺省的存量账号按 domain 兜底。
                # 界面靠它区分「国内版」与「国际版」，也靠它决定要不要显示
                # 地区注册入口——国际版没注册地区的号聊天会报 14017。
                account_realm = realm_mod.realm_of(doc.get("auth") or {})
                # realm_label / checkin_supported 都由 realm 模块给出，界面不自己推。
                # 国际版（workbuddy.ai）没有签到体系：点签到必然拿错误码，界面若
                # 照常显示「今日未确认签到」就会永远亮着一条消不掉的提示。
                row.update({"nickname": str(account.get("nickname") or ""), "uid": str(account.get("uid") or ""), "expires_at": expiry, "expired": expiry < time.time() * 1000, "refresh_available": bool(doc["auth"].get("refreshToken")), "status": "ready",
                            "is_enterprise": bool(enterprise_id), "enterprise_id": enterprise_id,
                            "realm": account_realm,
                            "realm_label": realm_mod.realm_label(account_realm),
                            "checkin_supported": realm_mod.supports_checkin(account_realm)})
            except (ValueError, OSError, KeyError):
                # 读不出来的记录仍然要有一份完整形状：前端少字段会渲染成 "undefined"。
                row.update({"status": "invalid", "expired": True, "expires_at": 0, "refresh_available": False,
                            "is_enterprise": False, "enterprise_id": "", "realm": realm_mod.CN,
                            "realm_label": realm_mod.realm_label(realm_mod.CN),
                            "checkin_supported": realm_mod.supports_checkin(realm_mod.CN)})
            result.append(row)
        return result

    def add_key(self, name, value=None):
        value = value or "sk-wb-" + secrets.token_urlsafe(32)
        kid = secrets.token_hex(8)
        self.data["keys"][kid] = {"name": name, "hash": digest(value), "hint": value[:5] + "…" + value[-4:], "created": int(time.time())}
        return kid, value

    def check_api(self, authorization, x_api_key):
        """校验下游 API Key：返回命中的密钥条目（含 id / 策略），否则抛 401。

        供 `converter._check_auth` 与 PoolMiddleware 复用。返回密钥是为了让
        池中间件在拿到 model / 来源 IP 后补做 IP·模型白名单与用量记账。
        """
        token = authorization[7:].strip() if authorization and authorization.startswith("Bearer ") else x_api_key or ""
        with self.lock:
            if digest(token) in self.test_keys:
                return None  # 临时测试密钥：无策略
        key = keysvc.resolve_key(self, token)
        if not key:
            raise HTTPException(401, "invalid api key")
        keysvc.validate_credential(key)  # 只校验凭据；IP / 模型在池中间件补
        return key

    def add_key(self, name, value=None):
        kid, token = keysvc.create_key(self, name, value=value)
        return kid, token


class AdminMiddleware:
    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or not scope["path"].startswith("/admin"):
            return await self.app(scope, receive, send)
        messages, size = [], 0
        while True:
            message = await receive()
            if message["type"] == "http.disconnect":
                return
            size += len(message.get("body", b""))
            if size > MAX_BODY:
                response = JSONResponse({"detail": "文件或请求超过 1 MB"}, 413)
                return await response(scope, receive, send)
            messages.append(message)
            if not message.get("more_body", False):
                break
        async def replay():
            return messages.pop(0) if messages else await receive()
        async def secure_send(message):
            if message["type"] == "http.response.start":
                message["headers"] = list(message.get("headers", [])) + [
                    (b"cache-control", b"no-store"), (b"x-content-type-options", b"nosniff"),
                    (b"referrer-policy", b"no-referrer"), (b"x-frame-options", b"DENY"),
                    (b"content-security-policy", b"default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
                ]
            await send(message)
        await self.app(scope, replay, secure_send)


class IPGuardMiddleware:
    """入站 IP 管控（全局白/黑名单）：启用时对 /v1 与 /admin 请求按规则放行/拦截。

    关闭时（默认）零开销直接穿透。规则命中与否写进 `ip_access_logs` 供安全页查看。

    三条**永远放行**的通道，都是为了「别把自己锁在门外」：

    · 本机回环地址（127.0.0.1 / ::1）。这一条是硬编码的，不是策略的一部分：
      所有部署方式里，能在服务器本机敲命令的人本来就能直接改 state.json，
      挡他没有任何安全收益，只会在用户存下一份写错的规则时把整个面板锁死。
      真正防的是外部来源，那部分策略照常生效。

    · 刚刚成功保存过安全策略的那个来源（`ip_last_allowed`）。保存成功这件事
      本身就是「这个来源此刻确实还进得来」的证据；把它一起挡掉，用户会失去
      唯一能修改规则的入口，只能去服务器上手工改 state.json。
      这条**只对带本人会话的请求生效**：会话是按来源 IP 记账的，换个来源
      拿着同一张票过来不算数，别人也伪造不了。匿名请求该挡就挡，否则
      「墙挡没挡住」这件事就没法验了。

    · 放行规则（allow）相关的三个端点。这条更实际：用户点「一键放行本机」
      的前提是他已经点不到任何页面了，只能靠这条后路把规则加回来。
      但**要求先给出一个有效身份**（管理令牌 / 管理密钥 / 本人会话）：只放
      allow 规则、不放任何别的写操作，且不给匿名者 —— 否则任何被墙挡住的人
      都能把自己加进白名单，「IP 管控」就只是一个摆设。
      身份不过关时一律 401，比 403 更准确：他缺的是身份，不是权限。
    """

    #: 被策略挡住时仍可访问的 admin 端点（方法, 路径片段）。
    SELF_RESCUE = (
        ("POST", "/admin/api/security/rules"),
        ("GET", "/admin/api/security/rules"),
        ("DELETE", "/admin/api/security/rules/"),
    )

    def __init__(self, app, store, sec=None):
        """`sec` 是 `Security` 实例（管理密钥与 legacy 开关都在它身上）。

        单独传进来而不是从 store 里翻：`Security` 是全局唯一的那一个，
        中间件判断「这个 X-Admin-Token 算不算身份」必须和它同一份真相。
        """
        self.app, self.store, self.sec = app, store, sec

    @staticmethod
    def _loopback(ip: str) -> bool:
        try:
            addr = ipaddress.ip_address((ip or "").strip())
        except ValueError:
            return False
        return addr.is_loopback

    def _rescue(self, method: str, path: str) -> bool:
        for verb, prefix in self.SELF_RESCUE:
            if method == verb and path.startswith(prefix):
                return True
        return False

    def _authorized(self, scope, ip: str) -> bool:
        """这个请求有没有一个有效身份（会话 / 管理令牌 / 管理密钥）。

        只看**来源是否对得上**：会话绑定签发时的来源 IP，换个地址拿同一张票
        过来不算数。这条规则和 `SecurityManager` 里的绑定检查同口径。

        拿到身份后**顺手把它挂到 `scope["state"]`**：中间件在这里已经解过一遍
        cookie 与令牌，再让下游的安全层用一模一样的口径解第二遍，除了浪费一次
        HMAC 与一次 PBKDF2，还会给「两套口径哪天改歪一个」留下缝隙。
        """
        headers = {k.decode().lower(): v.decode(errors="replace") for k, v in scope.get("headers", [])}
        if not headers.get("authorization") and not headers.get("x-admin-token"):
            return False
        sid, _ = security_mod.bearer_session_from_headers(headers)
        if sid:
            with self.store.lock:
                record = self.store.sessions.get(security_mod.digest(sid))
            if record and record.get("expires", 0) > time.time():
                bound = (record.get("ip") or "").strip()
                if bound and bound != ip:
                    return False
                scope.setdefault("state", {})["session"] = record
                return True
        key = headers.get("x-admin-token", "").strip()
        if key and self.sec is not None and self.sec.admin_key and hmac.compare_digest(key, self.sec.admin_key):
            if not self.sec.legacy_key_configured:
                return False
            scope.setdefault("state", {})["admin_key_ok"] = True
            return True
        return False

    def _log(self, ip: str, path: str, blocked: bool, reason: str, ua: str) -> None:
        try:
            with self.store.lock:
                logs = self.store.data.setdefault("ip_access_logs", [])
                logs.append({"ts": int(time.time()), "ip": ip, "path": path,
                             "blocked": bool(blocked), "reason": reason, "ua": ua[:120]})
                if len(logs) > IP_LOG_LIMIT:
                    del logs[:len(logs) - IP_LOG_LIMIT]
        except Exception:  # noqa: BLE001
            pass

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or scope.get("method") == "OPTIONS":
            return await self.app(scope, receive, send)
        path = scope.get("path", "")
        if not (path.startswith("/admin") or path.startswith("/v1")):
            return await self.app(scope, receive, send)
        cfg = self.store.data.get("security") or {}
        if not cfg.get("enabled"):
            return await self.app(scope, receive, send)
        ip = client_ip_from_scope(scope)
        # 本机直通，与身份无关：能在服务器上敲命令的人本来就能改 state.json。
        if self._loopback(ip):
            return await self.app(scope, receive, send)
        rules = self.store.data.get("ip_rules", [])
        allowed = evaluate(ip, rules, cfg.get("mode", "blacklist"))
        if not allowed:
            headers = {k.decode().lower(): v.decode(errors="replace") for k, v in scope.get("headers", [])}
            self._log(ip, path, True, "global", headers.get("user-agent", ""))
            # 两个例外，都是为了「别把自己锁在门外」，且都要求先给出有效身份：
            #   · 刚保存过策略的来源（ip_last_allowed）—— 它此刻确实还进得来；
            #   · 放行规则端点 —— 用户点「一键放行本机」的唯一后路。
            # 匿名者一个都不给：否则任何人被挡住之后都能把自己加回白名单，
            # 「IP 管控」就只是摆设。
            if self._trusted_here(ip, scope):
                return await self.app(scope, receive, send)
            if self._rescue(scope.get("method", ""), path):
                if self._authorized(scope, ip):
                    return await self.app(scope, receive, send)
                response = JSONResponse({"detail": "来源 IP 已被安全策略拒绝；"
                                                   "自救入口需要先登录（管理密钥或管理令牌）"}, 401)
                return await response(scope, receive, send)
            response = JSONResponse({"detail": "来源 IP 已被安全策略拒绝"}, 403)
            return await response(scope, receive, send)
        await self.app(scope, receive, send)

    def _trusted_here(self, ip: str, scope=None) -> bool:
        """这个来源是不是「刚刚成功保存过安全策略」的那一个。

        只认命中，不做通配：记录的是单个地址，比较也按字符串相等。
        **要求带本人会话或管理密钥**：换个来源拿同一张票过来不算数，
        因为会话绑定签发时的来源 IP。
        """
        saved = ((self.store.data.get("ip_last_allowed") or {}).get("value") or "").strip()
        if not saved or saved != (ip or "").strip():
            return False
        if scope is None:
            return True
        return self._authorized(scope, ip)


def block_metadata_target(url: str) -> None:
    """拒绝把上游地址指到云元数据服务。

    上游允许指任意主机是功能（用户自己的上游常在内网），但元数据地址
    （169.254.169.254 等）不是任何人的上游，指向它纯粹是攻击特征。
    触发这个出网请求的端点只要 viewer，挡掉它能把只读令牌的
    探测上限压到「只能探内网端口」。
    """
    host = urlparse(url).hostname or ""
    if not host:
        raise HTTPException(400, "上游地址缺少主机名")
    lowered = host.strip("[]").lower()

    if lowered in _METADATA_HOSTS:
        raise HTTPException(400, "不支持指向云元数据服务的上游地址")
    try:
        ip = ipaddress.ip_address(lowered)
    except ValueError:
        return
    # 169.254.0.0/16 整段是链路本地，元数据服务在里面。
    if ip.is_link_local:
        raise HTTPException(400, "不支持指向链路本地地址的上游")


#: 云元数据服务的常见名字与地址。写死是刻意的 —— 这张表不该被配置放宽。
_METADATA_HOSTS = frozenset({
    "metadata.google.internal", "metadata.goog",
    "169.254.169.254", "169.254.170.2", "fd00:ec2::254",
})


def _evict_stale_attempts(state: dict, now: int) -> int:
    """清掉**窗口已过期**的限流条目，返回清掉的条数。

    只删过期的，绝不删还在计数的 —— 这是这个函数存在的全部理由。

    早先的写法是「超过 500 条就按 window 排序削掉最老的 200 条」，
    而所有活跃条目的 window 都等于同一个 now，排序于是退化成插入序，
    正在被限流的那一条也会被削掉：换个密钥刷一轮就能把自己的限流
    清零。限流器被它要防的人关掉，等于没有。

    拆成独立函数是为了能直接测 —— 真实服务里 API Key 上限 100 枚、
    清理阈值 500，这条路径端到端根本走不到，走不到的东西就没法用
    HTTP 层的测试锁住。
    """
    expired = [k for k, v in state.items() if now - int(v.get("window", 0)) > 300]
    for key in expired:
        state.pop(key, None)
    return len(expired)

def _runtime_of(store) -> dict:
    """运行时长。优先用 store 自己的实现（SwitchStore.uptime 能给出累计值）。

    旧 `Store` 没有 uptime()——它只是个「账号存在 .info 里」的老实现，
    没有跨重启的累计概念。这里退化成「累计 == 本次会话」，
    保证接口在任何 store 下都返回同样形状的字段，前端不必分叉。
    """
    fn = getattr(store, "uptime", None)
    if callable(fn):
        try:
            return fn()
        except Exception:  # noqa: BLE001
            pass
    session = max(0, int(time.time() - getattr(store, "started", time.time())))
    return {"session": session, "total": session,
            "since": int(getattr(store, "started", time.time())),
            "total_since": int(getattr(store, "started", time.time()))}


def create_app(root=None, auth_dir=None, initial_key=None, admin_key=None, secure_cookie=True, store=None,
               legacy_key_configured=True):
    # store 可由调用方注入（例如 SwitchStore：账号来自 workbuddy-switch 的账号库）。
    # 不注入时保持原行为：账号来自本目录下的 .info 文件。
    if store is None:
        store = Store(root or os.environ.get("MANAGEMENT_DATA_DIR", "/data/management"), auth_dir or os.environ.get("CODEBUDDY_AUTH_DIR", "/data/auth"), initial_key if initial_key is not None else os.environ.get("CODEBUDDY2OPENAI_KEY", ""), admin_key or os.environ.get("ADMIN_KEY", ""))
    converter._check_auth = store.check_api
    converter.CONFIG.update({"desensitize": True, "no_compact": False, "log_path": None})
    browser_login = BrowserLogin(store.save_browser_account)
    pool = AccountPool(store)
    metrics = RequestMetrics()
    security = Security(store, admin_key if admin_key is not None
                        else (getattr(store, "admin_key", "") or os.environ.get("ADMIN_KEY", "")),
                        legacy_key_configured=legacy_key_configured)
    @asynccontextmanager
    async def lifespan(app):
        async def reap():
            while True:
                await asyncio.sleep(30)
                await browser_login.cleanup()
        cleanup_task = asyncio.create_task(reap())
        async def pool_worker():
            while True:
                try:
                    await pool.tick()
                except Exception:
                    pass  # Per-account errors are persisted without credential data.
                await asyncio.sleep(60)
        pool_task = asyncio.create_task(pool_worker())

        # 账号库热加载：workbuddy-switch 里增删账号后，服务无需重启。
        # 轮询代价只有一次 stat()（指纹不变就直接返回），默认 2 秒。
        async def accounts_worker():
            interval = max(0.5, float(os.environ.get("WB_ACCOUNTS_POLL", "2") or 2))
            while True:
                try:
                    await asyncio.to_thread(store.refresh_accounts)
                except Exception:
                    pass  # 账号库暂时不可读时保持现状，下一轮再试。
                await asyncio.sleep(interval)
        accounts_task = asyncio.create_task(accounts_worker())

        try:
            yield
        finally:
            cleanup_task.cancel()
            pool_task.cancel()
            accounts_task.cancel()
            try:
                await cleanup_task
            except asyncio.CancelledError:
                pass
            await browser_login.close()
            for task in (pool_task, accounts_task):
                try:
                    await task
                except asyncio.CancelledError:
                    pass
    app = FastAPI(docs_url=None, redoc_url=None, openapi_url=None, lifespan=lifespan)
    app.state.store = store
    app.state.security = security
    app.state.browser_login = browser_login
    app.state.pool = pool
    app.state.metrics = metrics
    app.add_middleware(AdminMiddleware)
    app.add_middleware(PoolMiddleware, pool=pool)
    app.add_middleware(MetricsMiddleware, metrics=metrics)
    app.add_middleware(IPGuardMiddleware, store=store, sec=security)
    # SPA（workbuddy-switch 前端）跨源调用 /admin/api/*：走 X-Admin-Token 头鉴权、不使用 cookie，
    # 因此可放开 CORS。最后添加 → 处于最外层，在 AdminMiddleware 之前处理预检 OPTIONS。
    # 可用 WB_ADMIN_CORS_ORIGINS 收紧来源（逗号分隔），默认放开。
    origins = [o.strip() for o in os.environ.get("WB_ADMIN_CORS_ORIGINS", "*").split(",") if o.strip()]
    app.add_middleware(
        CORSMiddleware,
        allow_origins=origins or ["*"],
        allow_credentials=False,
        # ⚠️ 必须包含 PUT —— 桌面端「服务设置 → 保存」走的就是 PUT
        # （src/lib/reverse-proxy.ts 的 saveSettings）。少了它，浏览器预检
        # 直接被拒 400，那个 PUT 根本不会发出去，界面表现是「点保存什么都不发生」，
        # 而后端日志里连一条请求都看不到 —— 因为请求确实没到达。
        #
        # 影响面只有桌面端：网页端同源、不触发预检，所以一直好用。
        # 后端测试也测不出来：TestClient 直连 ASGI，不发预检。
        #
        # 只补 PUT。不要改成 ["*"] —— 这里开的是跨源管理接口的面，
        # 凭空多放开 HEAD/TRACE 之类没有对接方的动词，只是白白扩大攻击面。
        allow_methods=["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"],
        allow_headers=["Content-Type", "X-Admin-Token", "X-CSRF-Token"],
        max_age=600,
    )

    async def payload(req):
        try:
            body = await req.json()
        except ValueError:
            raise HTTPException(400, "请求不是有效 JSON")
        if not isinstance(body, dict):
            raise HTTPException(400, "请求格式不正确")
        return body

    def log_inbound(ip: str, path: str, reason: str, ua: str, *, blocked: bool) -> None:
        """把一次入站访问写进 `ip_access_logs`（安全页看的那份）。

        与 IPGuardMiddleware 共用同一张表与上限，但**不受「IP 管控开关」影响**：
        安全策略没启用时也要留下「谁拿着错密钥在敲门」，否则这个缺口平时是隐形的。
        反过来，开关打开时由中间件统一记录，这里让路不重复写。
        """
        try:
            if (store.data.get("security") or {}).get("enabled"):
                return
            with store.lock:
                logs = store.data.setdefault("ip_access_logs", [])
                logs.append({"ts": int(time.time()), "ip": ip, "path": path,
                             "blocked": bool(blocked), "reason": reason, "ua": ua[:120]})
                if len(logs) > IP_LOG_LIMIT:
                    del logs[:len(logs) - IP_LOG_LIMIT]
        except Exception:  # noqa: BLE001
            pass

    # ── 角色依赖：viewer（任意身份）/ admin（管理员）/ session_admin（仅会话管理员）──
    def viewer(req: Request):
        return security.current_user(req)

    def admin(req: Request):
        return security.require_admin(req)

    def session_admin(req: Request):
        return security.require_session_admin(req)

    @app.get("/")
    async def home():
        return RedirectResponse("/admin/", 302)

    @app.get("/health")
    async def health():
        return {"status": "ok"}

    @app.get("/admin")
    async def redirect_admin():
        return RedirectResponse("/admin/", 302)

    @app.get("/admin/")
    async def index():
        return FileResponse(ASSETS / "index.html")

    @app.get("/admin/assets/{filename}")
    async def asset(filename: str):
        if filename not in {"app.js", "style.css"}:
            raise HTTPException(404)
        return FileResponse(ASSETS / filename)

    # 图标：没有它每个页面的控制台都会多一条 404，做前端检查时很容易
    # 被误当成「页面有资源加载失败」。用 SVG，省一个二进制文件。
    @app.get("/favicon.ico")
    async def favicon():
        return FileResponse(ASSETS / "favicon.svg", media_type="image/svg+xml")

    # ── Cookie 的 Secure 标志：跟着请求实际是不是 HTTPS 走 ──────
    #
    # 原先是硬编码 True。后果不是「更安全」，而是登录直接失效：
    # 客户端（浏览器、httpx）拒绝把 Secure cookie 发回明文 HTTP，于是
    # 登录返回 200、cookie 也下发了，但后续每个请求都 401 ——
    # 界面表现是「登录成功后立刻弹回登录页」，且没有任何报错可查。
    #
    # 本服务默认就监听 127.0.0.1，明文是它的正常形态（见 serve.py 的
    # `--host` 默认值），所以这不是边缘场景，是主路径。
    #
    # 仍然保留 Secure：只要经过 HTTPS 反代（VPS 上挂 Caddy / Nginx），
    # 或者显式配 `WB_SECURE_COOKIE=1`，cookie 就带 Secure，
    # 不会为了本机调试方便而把线上部署的安全性降下来。

    def _cookie_secure(req: Request) -> bool:
        """管理会话 cookie 要不要带 Secure。"""
        override = os.environ.get("WB_SECURE_COOKIE", "").strip().lower()
        if override in ("1", "true", "yes", "on"):
            return True
        if override in ("0", "false", "no", "off"):
            return False
        # X-Forwarded-Proto 由反代设置；uvicorn 的 proxy_headers 也会读它。
        proto = (req.headers.get("x-forwarded-proto") or "").split(",")[0].strip().lower()
        if proto:
            return proto == "https"
        return req.url.scheme == "https"

    def _login_response(csrf: str, sid: str, req: Request) -> JSONResponse:
        """登录成功：下发会话 cookie + 响应体里的 csrf / token。

        `token` 是 `X-Admin-Token` 的值，供**跨源调用方**用（桌面端 SPA 走
        `credentials:"omit"`，SameSite=strict 的 cookie 根本带不上）。

        它和 cookie 是同一份凭据，所以这里并不额外扩大暴露面 ——
        能读到这个响应的脚本本来就已经在同源上下文里了。但要注意
        `X-Admin-Token` 那条路**跳过 CSRF**（见 security.bearer_session），
        因此它的寿命与注销行为必须可靠：`/admin/api/logout` 会立刻失效
        （有测试锁着），未注销时最长 12 小时。
        """
        response = JSONResponse({"csrf": csrf, "token": sid})
        response.set_cookie(COOKIE, sid, secure=_cookie_secure(req), httponly=True,
                            samesite="strict", path="/admin", max_age=12 * 3600)
        return response

    @app.post("/admin/api/login")
    async def login(req: Request):
        body = await payload(req)
        ip = client_ip(req)
        username = str(body.get("username") or "").strip()[:64]

        # 管理令牌（wbt_…）换会话：让浏览器控制台也能用令牌进门。
        # 放在最前面判，因为令牌登录的请求体里既没有 key 也没有 username，
        # 落到下面那个兼容分支只会得到「管理密钥不正确」这种驴唇不对马嘴的报错。
        #
        # 安全性上没放宽：令牌本来就能用 `Authorization: Bearer` 调 API，所以
        # 「能进控制台」没有多给任何权限，角色照样按令牌 scope 定（只读令牌进得去、
        # 改不动东西）。而「改用户 / 改令牌」这类高危接口仍然拒绝——见
        # `require_session_admin`，它靠会话记录里的 token_id 认出这条路。
        if "token" in body:
            value = str(body.get("token") or "").strip()
            if security.login_blocked(ip):
                raise HTTPException(429, "尝试次数过多，请 10 分钟后再试")
            row = security.resolve_token(value)
            if not row or not security.token_usable(row):
                security.record_fail(ip)
                security.audit({"username": "(管理令牌)"}, "login_failed", "",
                               f"管理令牌无效、已停用或已过期；来源 {ip}")
                raise HTTPException(401, "管理令牌无效、已停用或已过期")
            security.clear_fail(ip)
            actor = f"token:{row['name']}"
            sid, csrf = security.start_session(actor, scope_to_role(row["scope"]),
                                               via="token", token_id=row["id"], ip=ip)
            security.audit({"username": actor}, "login", row["name"],
                           f"管理令牌登录；来源 {ip}")
            # 令牌换来的会话照常种 cookie：内置静态后台就是靠它维持登录态的
            # （见 tests/test_console_login.py::test_token_login_grants_console_session）。
            # 曾经想「令牌是凭据、不必再种 cookie」，那会把控制台的令牌登录打断 ——
            # 登录返回 200，之后每个请求 401。
            return _login_response(csrf, sid, req)

        if "key" in body or not username:
            # 兼容旧形态：单密钥 ADMIN_KEY 登录（内置静态后台）。保留原有「按 IP 8 次锁定」行为。
            value = body.get("key")
            now = time.time()
            with store.lock:
                store.attempts = {k: v for k, v in store.attempts.items() if v[-1] > now - 600}
                failures = [t for t in store.attempts.get(ip, []) if t > now - 600]
                if len(failures) >= 8:
                    raise HTTPException(429, "尝试次数过多，请 10 分钟后重试")
                if not security.verify_legacy_key(value):
                    store.attempts[ip] = failures + [now]
                    raise HTTPException(401, "管理密钥不正确")
                store.attempts.pop(ip, None)
            sid, csrf = security.start_session("admin", "admin", via="session", ip=ip)
            security.audit({"username": "admin"}, "login", "admin", f"管理密钥登录；来源 {ip}")
            return _login_response(csrf, sid, req)

        # 多用户：用户名 + 口令（PBKDF2 校验）。
        password = str(body.get("password") or "")
        if security.login_blocked(ip, username):
            raise HTTPException(429, "失败次数过多，请 10 分钟后再试")
        user = security.verify_login(username, password)
        if not user:
            security.record_fail(ip, username)
            security.audit({"username": username or "(空)"}, "login_failed", username, f"来源 {ip}")
            raise HTTPException(401, "用户名或密码错误")
        security.clear_fail(ip, username)
        sid, csrf = security.start_session(user["username"], user["role"], via="session", ip=ip)
        security.audit(user, "login", user["username"], f"来源 {ip}")
        return _login_response(csrf, sid, req)

    @app.get("/admin/api/session")
    async def session(req: Request):
        user = security.current_user(req)
        return {"csrf": security.session_csrf(req), "username": user["username"],
                "role": user["role"], "via": user.get("via"),
                # 版本号也跟着会话发一份（登录后前端会读这个接口）。
                "version": _version.version(),
                "dev": _version.is_dev()}

    @app.get("/admin/api/login-options")
    async def login_options():
        """登录页能摆哪些入口——**不需要登录**，登录页本身要据此渲染。

        只暴露「这个入口在当前部署下有没有用」，不含任何凭据、也不透露用户名。

        版本号放在这里，是因为**这个接口是唯一保证未登录也能访问的**。
        原先想挂在 `/admin/api/session` 上，实测它未登录时返回 401 ——
        那样登录页永远拿不到版本，而"登不上去"恰恰是最需要版本的场景。

        版本号不是敏感信息：它只说明"这是哪个构建"，不透露任何配置或凭据。
        """
        return {"legacy_key": security.legacy_key_enabled(),
                "tokens": security.has_tokens(),
                "version": _version.version(),
                "dev": _version.is_dev()}

    @app.get("/admin/api/me")
    async def me(req: Request):
        user = viewer(req)
        return {"username": user["username"], "role": user["role"], "via": user.get("via")}

    @app.post("/admin/api/logout")
    async def logout(req: Request):
        """登出：**必须幂等**。

        原先第一行是 `security.current_user(req)`，而它在本机已经没登录时会直接
        抛 401。于是「登出」这个动作本身变成了「只有已登录才能登出」——
        听起来像句废话，实际会造成两个真问题：

          1. 桌面端点「退出」时，token 已经被上次登出清掉了，这一发就是 401；
             前端把它当失败弹 toast，用户看到的是「退不出去 / 报错」。
          2. 会话已经过期 / 被吊销之后再点退出，同样 401。

        登出的语义是「把当前的登录态清干净」，**没登录本来就是目标状态**，
        没有什么可失败的。所以这里不再要求先通过鉴权：拿到什么就清什么，
        永远回 200。cookie 照删（清掉可能残留的那一份）。
        """
        sid, _ = bearer_session(req)
        if sid:
            # cancel_owner / end_session 都是按 sid 清，sid 不存在时是空操作。
            await browser_login.cancel_owner(digest(sid))
            security.end_session(req)
        response = JSONResponse({"ok": True})
        # 无论上面走没走到，都把 cookie 删掉——浏览器里可能还留着上一次的。
        security.delete_cookie(response, secure_cookie)
        return response

    @app.post("/admin/api/sessions/revoke")
    async def revoke_sessions(req: Request):
        """吊销**当前用户**的全部会话（含本次），并清除本机 cookie。"""
        actor = session_admin(req)
        n = security.revoke_user_sessions(actor["username"])
        security.audit(actor, "revoke_sessions", actor["username"], f"吊销 {n} 个会话；来源 {client_ip(req)}")
        response = JSONResponse({"ok": True, "revoked": n, "relogin_required": True})
        security.delete_cookie(response, secure_cookie)
        return response

    # ── 管理用户（仅会话管理员）────────────────────────────────
    @app.get("/admin/api/users")
    async def list_users(req: Request):
        # admin 而不是 viewer：这份清单是控制台账号 + 角色，
        # 正是登录爆破需要的输入。同段的写接口本来都是 session_admin。
        admin(req)
        return security.public_users()

    @app.post("/admin/api/users")
    async def add_user(req: Request):
        actor = session_admin(req)
        body = await payload(req)
        result = security.add_user(body.get("username", ""), body.get("password", ""), body.get("role", "viewer"))
        security.audit(actor, "add_user", result["username"],
                       f"角色={result['role']}；来源 {client_ip(req)}")
        return result

    @app.patch("/admin/api/users/{username}")
    async def edit_user(username: str, req: Request):
        actor = session_admin(req)
        body = await payload(req)
        result = security.update_user(username, body.get("password"), body.get("role"))
        security.audit(actor, "update_user", username, f"来源 {client_ip(req)}")
        result["relogin_required"] = result["sessions_revoked"] and username == actor["username"]
        if result["relogin_required"]:
            security.end_session(req)
        return result

    @app.delete("/admin/api/users/{username}")
    async def remove_user(username: str, req: Request):
        actor = session_admin(req)
        result = security.delete_user(username, actor["username"])
        security.audit(actor, "delete_user", username, f"来源 {client_ip(req)}")
        return result

    # ── 作用域化管理令牌（wbt_；仅会话管理员可增删改）─────────
    @app.get("/admin/api/tokens")
    async def list_tokens(req: Request):
        # 令牌自身的接口一律只收会话管理员：泄露的令牌不能用来枚举其它令牌。
        session_admin(req)
        return security.list_tokens()

    @app.post("/admin/api/tokens")
    async def add_token(req: Request):
        actor = session_admin(req)
        body = await payload(req)
        created = security.create_token(body.get("name", ""), body.get("scope", "readonly"),
                                        body.get("expires_at"), created_by=actor["username"])
        security.audit(actor, "create_token", created["name"],
                       f"scope={created['scope']}；来源 {client_ip(req)}")
        return created

    @app.patch("/admin/api/tokens/{tid}")
    async def edit_token(tid: str, req: Request):
        actor = session_admin(req)
        body = await payload(req)
        updated = security.update_token(tid, body)
        security.audit(actor, "update_token", updated["name"], f"来源 {client_ip(req)}")
        return updated

    @app.delete("/admin/api/tokens/{tid}")
    async def remove_token(tid: str, req: Request):
        actor = session_admin(req)
        result = security.delete_token(tid)
        security.audit(actor, "delete_token", tid, f"来源 {client_ip(req)}")
        return result

    # ── 入站 IP 管控 ────────────────────────────────────────────
    @app.get("/admin/api/security/config")
    async def security_config(req: Request):
        viewer(req)
        with store.lock:
            cfg = dict(store.data.get("security") or {"enabled": False, "mode": "blacklist"})
            # `current_ip` / `last_allowed_ip` 只用于界面上的自救提示，不是策略的一部分。
            # 白名单模式保存之后本机很可能就此失联，用户需要知道要放行哪个地址 ——
            # 而这个地址只能由*现在*还连得上的这次请求告诉它。
            cfg["current_ip"] = client_ip(req)
            cfg["last_allowed_ip"] = (store.data.get("ip_last_allowed") or {}).get("value") or ""
            return cfg

    @app.post("/admin/api/security/config")
    async def write_security_config(req: Request):
        actor = session_admin(req)
        body = await payload(req)
        mode = body.get("mode", "blacklist")
        if mode not in ("whitelist", "blacklist"):
            raise HTTPException(400, "模式无效")
        cfg = {"enabled": bool(body.get("enabled")), "mode": mode}
        with store.lock:
            # 保存「上一次真的被放行过」的 IP。白名单模式一旦保存成功，本机如果
            # 不在放行名单里，**下一次请求就是 403** —— 用户会立刻失去这个页面，
            # 也就失去了把自己放回来的入口，只能去服务器上手工改 state.json。
            # 记下来还不够（用户可能在别的机器上操作），所以 GET /security/config
            # 会把它一并回传，由界面顶一条「继续会被挡」的红条。
            previous = dict(store.data.get("security") or {})
            store.data["security"] = cfg
            if cfg["enabled"] or previous.get("mode") != cfg["mode"]:
                # 只在「策略可能变严」的两种情况下记：一是刚启用，二是刚切模式。
                # 关掉开关或同一模式下点保存，没有失联风险，不必覆盖这份备份。
                store.data.setdefault("ip_last_allowed", {})["value"] = client_ip(req)
            store.save()
        if cfg["enabled"]:
            # 中间件只认来源 IP，不认会话：一个刚被挡在门外的来源，手里若还留着
            # 之前发出去的会话，每个请求都会 401 —— 而 401 会走 `record_fail()`，
            # 连挂几次就把这个 IP 送进 10 分钟登录锁定，于是「刚把他挡住」变成
            # 「他现在连正确口令都登不进来」。所以收紧策略的同时，把门外来源的
            # 会话和失败计数一起清掉：挡就是挡干净，不留一条半开的缝。
            #
            # **但当前这次请求的来源绝不能踢**：保存策略的那一次本身就是
            # 「这个来源还进得来」的证据。把它踢掉，操作者会在点下保存的瞬间
            # 失去自己，而这时他已经点不到任何页面去救自己了。
            here = client_ip(req)
            rules = list(store.data.get("ip_rules", []))
            mode = cfg["mode"]
            for known in security.session_ips():
                if known != here and not evaluate(known, rules, mode):
                    security.revoke_sessions_from(known)
        security.audit(actor, "update_security", "",
                       f"enabled={cfg['enabled']} mode={cfg['mode']}；来源 {client_ip(req)}")
        return cfg

    @app.get("/admin/api/security/rules")
    async def list_ip_rules(req: Request):
        viewer(req)
        with store.lock:
            return list(store.data.get("ip_rules", []))

    @app.post("/admin/api/security/rules")
    async def add_ip_rule(req: Request):
        actor = session_admin(req)
        body = await payload(req)
        kind = body.get("kind")
        cidr = str(body.get("cidr") or "").strip()
        if kind not in ("allow", "deny"):
            raise HTTPException(400, "规则类型无效")
        if not valid_cidr(cidr):
            raise HTTPException(400, f"非法的 IP / CIDR：{cidr}")
        with store.lock:
            rid = secrets.token_hex(6)
            rule = {"id": rid, "kind": kind, "cidr": cidr,
                    "note": str(body.get("note") or "")[:120], "created_at": int(time.time())}
            store.data.setdefault("ip_rules", []).insert(0, rule)
            store.save()
        security.audit(actor, "add_ip_rule", cidr, f"{kind}；来源 {client_ip(req)}")
        return rule

    @app.delete("/admin/api/security/rules/{rid}")
    async def remove_ip_rule(rid: str, req: Request):
        actor = session_admin(req)
        with store.lock:
            rules = store.data.get("ip_rules", [])
            if not any(r["id"] == rid for r in rules):
                raise HTTPException(404, "规则不存在")
            store.data["ip_rules"] = [r for r in rules if r["id"] != rid]
            store.save()
        security.audit(actor, "delete_ip_rule", rid, f"来源 {client_ip(req)}")
        return {"ok": True}

    @app.get("/admin/api/security/logs")
    async def ip_access_logs(req: Request, limit: int = 200):
        viewer(req)
        with store.lock:
            logs = list(store.data.get("ip_access_logs", []))
        logs.reverse()
        return logs[: max(1, min(IP_LOG_LIMIT, limit))]

    @app.post("/admin/api/security/logs/clear")
    async def clear_ip_logs(req: Request):
        actor = session_admin(req)
        with store.lock:
            store.data["ip_access_logs"] = []
            store.save()
        security.audit(actor, "clear_ip_logs", "", f"来源 {client_ip(req)}")
        return {"ok": True}

    @app.get("/admin/api/audit-logs")
    async def audit_logs(req: Request, limit: int = 200):
        # admin：审计日志含全部运维动作与 PII，只读身份不该看到。
        admin(req)
        return security.audit_logs(limit)

    @app.post("/admin/api/audit-logs/clear")
    async def clear_audit_logs(req: Request):
        actor = session_admin(req)
        removed = security.clear_audit_logs()
        # 清空动作本身要留痕：先清空再记录，新日志里就只剩这一条。
        security.audit(actor, "clear_audit_logs", "", f"清空 {removed} 条；来源 {client_ip(req)}")
        return {"ok": True, "removed": removed}

    @app.post("/admin/api/oauth/start")
    async def oauth_start(req: Request):
        admin(req)
        body = await payload(req)
        name = clean_name(body.get("name"), None)
        # realm/region 从 body 取：这是“这一版登录”的意图，不是账号的持久属性。
        # 允许 query 兜底只是为了 curl 方便；body 优先，避免管理端顺手加个同名
        # query 就把 body 静默盖掉（上游踩过这个坑）。
        realm = realm_mod.norm_realm(body.get("realm") or req.query_params.get("realm"))
        region = body.get("region") or req.query_params.get("region")
        # slot 只在批量扫码里传：同一个 slot 重发码会顶掉旧的，见 browser_login.start。
        slot = body.get("slot")
        owner, _ = bearer_session(req)
        try:
            return await browser_login.start(digest(owner), name, realm, region, slot)
        except (httpx.HTTPError, ValueError, TypeError):
            raise HTTPException(502, "授权服务连接失败，请稍后重试")

    @app.post("/admin/api/oauth/{fid}/poll")
    async def oauth_poll(fid: str, req: Request):
        admin(req)
        owner, _ = bearer_session(req)
        try:
            return await browser_login.poll(fid, digest(owner))
        except (httpx.HTTPError, ValueError, TypeError):
            raise HTTPException(502, "授权状态暂时无法获取，请稍后重试")

    @app.delete("/admin/api/oauth/{fid}")
    async def oauth_cancel(fid: str, req: Request):
        admin(req)
        owner, _ = bearer_session(req)
        return await browser_login.cancel(fid, digest(owner))

    #: 批量扫码一次最多摆多少张码。
    #:
    #: 50 不是拍脑袋：`browser_login.MAX_FLOWS_PER_OWNER` 也是 50，两处必须
    #: 一致——否则这里允 100、那边在 51 张开始 429，界面会出现「前 50 张好了，
    #: 后面全红」这种看起来像上游抽风的半成功状态。
    BATCH_SCAN_MAX = 50

    @app.post("/admin/api/oauth/batch/start")
    async def oauth_batch_start(req: Request):
        """批量生成授权链接：一次开 N 张码，供用户连着扫。

        为什么不是「一个接口循环调 N 次 /oauth/start」：
        `start()` 不传 slot 时会 `cancel_owner(owner)`（单账号登录时代的口径：
        一个会话只该有一张码）。批量场景下第二次调用会把第一次的码干掉，
        循环 N 次的结果是只有最后一张活着。所以必须显式带 slot。

        部分失败是**预期结果**而非异常：上游偶发 502 时不该让整批失败，
        用户已经扫码的那几张不该被回滚。返回 `items` 里逐条给 items.status。
        """
        admin(req)
        body = await payload(req)
        # 只有**键不存在**或为空才算「没给」（默认 1 张）。写成
        # `int(body.get("count") or 1)` 会把显式的 0 谎报成 1 —— 调用方要么
        # 以为一张都没开，要么看到凭空多出一张，两种都不对。
        raw_count = body.get("count")
        if raw_count is None or raw_count == "":
            raw_count = 1
        try:
            count = int(raw_count)
        except (TypeError, ValueError):
            raise HTTPException(400, "数量必须是整数")
        if count < 1:
            raise HTTPException(400, "数量至少为 1")
        if count > BATCH_SCAN_MAX:
            raise HTTPException(400, f"一次最多生成 {BATCH_SCAN_MAX} 张登录码")
        realm = realm_mod.norm_realm(body.get("realm"))
        region = str(body.get("region") or "").strip().upper()
        if region and region not in realm_mod.REGION_CODES:
            raise HTTPException(400, f"不支持的地区：{region}")
        prefix = str(body.get("prefix") or "批量").strip()[:40] or "批量"
        owner, _ = bearer_session(req)
        owner_digest = digest(owner)
        items = []
        for index in range(count):
            slot = f"b{index}"
            try:
                flow = await browser_login.start(
                    owner_digest, f"{prefix}{index + 1}", realm, region or None, slot)
                items.append({"slot": slot, "status": "ready", **flow})
            except HTTPException as exc:
                items.append({"slot": slot, "status": "failed", "error": str(exc.detail)})
            except (httpx.HTTPError, ValueError, TypeError):
                items.append({"slot": slot, "status": "failed", "error": "授权服务连接失败"})
        ready = sum(1 for item in items if item["status"] == "ready")
        security.audit(owner, "oauth_batch_start", f"{ready}/{count}", f"realm={realm}")
        return {"items": items, "ready": ready, "count": count, "realm": realm,
                "region": region, "ttl": browser_login_mod.TTL}
    #: 只读身份可以看到的 pool 字段。**白名单**，不是黑名单。
    #: pool 段里存着 wb2api_key（发给上游的 Bearer 凭据），
    #: 而端点只要 viewer —— 用白名单是因为这个 store 段谁都能往里加键，
    #: 以后加了新凭据字段，黑名单会默默漏出去。
    POOL_PUBLIC_KEYS = (
        "routing", "spread", "spread_minutes",
        "auto_checkin", "checkin_time",
        "auto_travel", "travel_time",
        "auto_keepalive", "keepalive_hours",
        "auto_active", "active_time", "active_reports",
        # 地址不是凭据，界面要显示"连的是哪个上游"；key 绝不回显。
        "wb2api_base",
    )

    def _pool_view(store, *, reveal_secret: bool = False) -> dict:
        """`pool` 段 + 一条**上游读取状态**。

        `wb2api_error` 非空 = 这次（或最近一次）读上游 `/status` 失败了，
        界面据此把「冷却状态未知」讲出来，而不是画成「没有冷却」。

        放在 pool 段而不是每行账号里：这个错误是**整批共享**的 ——
        `upstream_cooldowns()` 一次调用拿整张 uid→冷却 的表。
        逐行重复同一个错误字符串既冗余，又会让「是不是每个号各有各的错」
        这种误读变得可能。

        注意：**这里不碰 `up_cooling`**。那仍然是 None（未知），
        绝不因为读不到就退化成 False（无冷却）—— 那正是这个缺陷的成因。
        """
        raw = store.data["pool"]
        if reveal_secret:
            view = dict(raw)
        else:
            view = {k: raw[k] for k in POOL_PUBLIC_KEYS if k in raw}
        view["wb2api_error"] = pool.upstream_error()
        return view


    @app.get("/admin/api/overview")
    async def overview(req: Request):
        actor = viewer(req)
        # ⚠️ 密钥的**明文**只给管理员。
        #
        # 这个端点原先无条件内联 `keysvc.list_keys(store)`，而它带回明文
        # `secret`；端点本身只要 viewer，于是持 readonly 身份的人
        # （含一枚最低权限 wbt_ 令牌）能读到全站所有密钥明文 ——
        # 拿到即可直接调用上游，等于一次性拿走所有人的额度。
        # 已用真实 readonly 令牌 PoC 坐实。
        #
        # 按角色决定，而不是把这个端点整体提权到 admin：
        # viewer 本来就该能看用量/账号概览（面板的只读页要靠它），
        # 提权会把只读面板整个打挂；真正敏感的只有密钥明文那一项。
        reveal = str((actor or {}).get("role") or "") == "admin"
        with store.lock:
            return {"accounts": pool.rows(store.account_rows()),
                    "pool": _pool_view(store, reveal_secret=reveal),
                    "metrics": metrics.snapshot(),
                    "keys": keysvc.list_keys(store, reveal_secret=reveal),
                    "models": converter.get_available_models(),
                    # 客户端没带 model 时反代替它选的那一个（已按本机可用清单解析过）。
                    # 界面拿它当测试表单的默认选中项，也是「默认模型」这件事的唯一出处。
                    "default_model": converter.resolve_default_model(),
                    "checkin_logs": pool.checkin_logs(20),
                    "request_logs": pool.request_logs(50),
                    # 分组也放进来：密钥表单要能勾选分组，密钥列表要能把 id 显示成组名。
                    # 单独再发一次 /admin/api/groups 会让「打开密钥页」多一次往返，
                    # 而这份清单很小（≤32 条）。
                    "groups": groups.list_groups(store),
                    # `uptime` 保留为整数秒（老前端/老脚本在用），
                    # 新增 `runtime` 给出 session / total 两个口径，见 SwitchStore.uptime()。
                    "uptime": int(time.time() - store.started),
                    # 两个 store 实现都要能用：SwitchStore 有累计运行时间，
                    # 旧的 Store（账号来自 .info 文件）没有——不能让它 AttributeError，
                    # 否则所有走旧 store 的接口一起 500。缺的时候退化成「只有本次会话」。
                    "runtime": _runtime_of(store),
                    "events": list(store.events)}

    @app.post("/admin/api/accounts/{aid}/actions/{action}")
    async def account_action(aid: str, action: str, req: Request):
        admin(req)
        if action not in ("refresh", "status", "checkin", "travel", "travel-claim", "active"):
            raise HTTPException(404)
        live_accounts(store)
        return await asyncio.to_thread(pool.operate, aid, action)

    @app.post("/admin/api/pool/actions/{action}")
    async def pool_action(action: str, req: Request):
        admin(req)
        if action not in ("status", "checkin", "travel", "travel-claim", "active"):
            raise HTTPException(404)
        return await pool.batch(action)

    @app.patch("/admin/api/pool/settings")
    async def pool_settings(req: Request):
        actor = admin(req)
        body = await payload(req)
        import re
        with store.lock:
            settings = dict(store.data["pool"])
            if "routing" in body:
                if body["routing"] not in ("manual", "round_robin", "least_used"):
                    raise HTTPException(400, "调度方式无效")
                settings["routing"] = body["routing"]
            if "spread" in body:
                if type(body["spread"]) is not bool:
                    raise HTTPException(400, "轮换驻留开关无效")
                settings["spread"] = body["spread"]
            if "spread_minutes" in body:
                try:
                    minutes = int(body["spread_minutes"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "驻留时长需为整数分钟")
                if not 1 <= minutes <= 1440:
                    raise HTTPException(400, "驻留时长需在 1–1440 分钟之间")
                settings["spread_minutes"] = minutes
            if "auto_checkin" in body:
                if type(body["auto_checkin"]) is not bool:
                    raise HTTPException(400, "签到开关无效")
                settings["auto_checkin"] = body["auto_checkin"]
            if "checkin_time" in body:
                if not isinstance(body["checkin_time"], str) or not re.fullmatch(r"(?:[01]\d|2[0-3]):[0-5]\d", body["checkin_time"]):
                    raise HTTPException(400, "请选择有效的签到时间")
                settings["checkin_time"] = body["checkin_time"]
            if "auto_travel" in body:
                if type(body["auto_travel"]) is not bool:
                    raise HTTPException(400, "派猫猫旅行开关无效")
                settings["auto_travel"] = body["auto_travel"]
            if "travel_time" in body:
                if not isinstance(body["travel_time"], str) or not re.fullmatch(r"(?:[01]\d|2[0-3]):[0-5]\d", body["travel_time"]):
                    raise HTTPException(400, "请选择有效的派旅行时间")
                settings["travel_time"] = body["travel_time"]
            if "auto_keepalive" in body:
                if type(body["auto_keepalive"]) is not bool:
                    raise HTTPException(400, "保活开关无效")
                settings["auto_keepalive"] = body["auto_keepalive"]
            if "keepalive_hours" in body:
                try:
                    hours = int(body["keepalive_hours"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "保活提前量需为整数小时")
                # 上限 168 小时（一周）：比 token 自身的寿命还长的提前量没有意义，
                # 只会让每个 tick 都去刷一遍。
                if not 1 <= hours <= 168:
                    raise HTTPException(400, "保活提前量需在 1–168 小时之间")
                settings["keepalive_hours"] = hours
            if "auto_active" in body:
                if type(body["auto_active"]) is not bool:
                    raise HTTPException(400, "活跃上报开关无效")
                settings["auto_active"] = body["auto_active"]
            if "active_time" in body:
                if not isinstance(body["active_time"], str) or not re.fullmatch(r"(?:[01]\d|2[0-3]):[0-5]\d", body["active_time"]):
                    raise HTTPException(400, "请选择有效的活跃上报时间")
                settings["active_time"] = body["active_time"]
            if "active_reports" in body:
                try:
                    reports = int(body["active_reports"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "上报条数需为整数")
                # 上限 5：领养猫的对话量门槛就是 5 次，报更多没有额外收益，
                # 只是把同一个账号在同一分钟里对着上游多打几次。
                if not 1 <= reports <= 5:
                    raise HTTPException(400, "上报条数需在 1–5 之间")
                settings["active_reports"] = reports
            # 上游 workbuddy2api 接入点：填了才去读它的模型级冷却。
            # 空串是合法值（= 不读），所以不能用「真值判断」跳过——
            # 那样用户就永远清不掉这个配置了。
            if "wb2api_base" in body:
                base = body["wb2api_base"]
                if not isinstance(base, str):
                    raise HTTPException(400, "上游地址需为字符串")
                base = base.strip().rstrip("/")
                if base and not re.fullmatch(r"https?://[^\s/]+(?::\d+)?(?:/[^\s]*)?", base):
                    raise HTTPException(400, "上游地址需形如 http://127.0.0.1:7863")
                if base:
                    block_metadata_target(base)
                settings["wb2api_base"] = base
            if "wb2api_key" in body:
                key = body["wb2api_key"]
                if not isinstance(key, str):
                    raise HTTPException(400, "上游密钥需为字符串")
                settings["wb2api_key"] = key.strip()
            # ── 账号级流量整形的默认值（新号初值，也是全池统改的入口）──
            #
            # 校验口径与单账号那三个字段一致，但**下限不同**：
            # 这里的权重必须是正数。把「默认权重」设成 0 等于让新加的号
            # 一律不参与轮转——加完发现不工作，且没有任何提示。
            # 「不参与轮转」是单账号的语义，不是默认值的语义。
            if "default_weight" in body:
                try:
                    weight = float(body["default_weight"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "默认权重需为数字")
                if weight != weight or weight in (float("inf"), float("-inf")):
                    raise HTTPException(400, "默认权重需为有限数字")
                if not 0.1 <= weight <= 100:
                    raise HTTPException(400, "默认权重需在 0.1–100 之间")
                settings["default_weight"] = round(weight, 2)
            if "default_max_concurrency" in body:
                try:
                    limit = int(body["default_max_concurrency"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "默认并发上限需为整数")
                if not 0 <= limit <= throttle.MAX_CONCURRENCY:
                    raise HTTPException(
                        400, f"默认并发上限需在 0–{throttle.MAX_CONCURRENCY} 之间")
                settings["default_max_concurrency"] = limit
            if "default_min_interval" in body:
                try:
                    gap = float(body["default_min_interval"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "默认最小间隔需为数字")
                if gap != gap or gap in (float("inf"), float("-inf")):
                    raise HTTPException(400, "默认最小间隔需为有限数字")
                if not 0 <= gap <= throttle.MAX_MIN_INTERVAL:
                    raise HTTPException(
                        400, f"默认最小间隔需在 0–{int(throttle.MAX_MIN_INTERVAL)} 秒之间")
                settings["default_min_interval"] = round(gap, 2)
            # 全池总并发上限。允许 0（不限制）——这一项和默认权重不同：
            # 「不限制」本来就是它的缺省语义，不需要挡住。
            if "max_total_concurrency" in body:
                try:
                    gate = int(body["max_total_concurrency"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "全池并发上限需为整数")
                if not 0 <= gate <= throttle.MAX_CONCURRENCY:
                    raise HTTPException(
                        400, f"全池并发上限需在 0–{throttle.MAX_CONCURRENCY} 之间")
                settings["max_total_concurrency"] = gate
            # ── 镜像加速（重试 / 退避）──
            # 三项一起用：enabled 是总开关，attempts 是**总尝试次数**（含第一次），
            # backoff 是首次退避基数（指数增长 + 抖动，上限见 mirror 模块）。
            # 关掉时把另外两项也清成默认值，避免界面上看起来"还开着"但实际不生效。
            if "retry_enabled" in body:
                enabled = bool(body["retry_enabled"])
                settings["retry_enabled"] = enabled
                if not enabled:
                    settings["retry_attempts"] = 0
                    settings["retry_backoff"] = 0.0
            if "retry_attempts" in body:
                try:
                    attempts = int(body["retry_attempts"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "重试次数需为整数")
                if not 0 <= attempts <= mirror.MAX_ATTEMPTS_CAP:
                    raise HTTPException(
                        400, f"重试次数需在 0–{mirror.MAX_ATTEMPTS_CAP} 之间")
                settings["retry_attempts"] = attempts
            if "retry_backoff" in body:
                try:
                    backoff = float(body["retry_backoff"])
                except (TypeError, ValueError):
                    raise HTTPException(400, "退避基数需为数字")
                if not math.isfinite(backoff) or backoff < 0:
                    raise HTTPException(400, "退避基数需为有限的非负数")
                if backoff > mirror.MAX_BACKOFF_CAP:
                    raise HTTPException(
                        400, f"退避基数最大 {mirror.MAX_BACKOFF_CAP:g} 秒")
                settings["retry_backoff"] = round(backoff, 2)
            store.data["pool"] = settings
            store.save()
        # 改了地址/密钥必须立刻生效，不能还拿旧上游的缓存糊弄界面。
        pool._upstream_cache = ({}, None, 0.0)
        editable = ('routing', 'spread', 'spread_minutes', 'auto_checkin', 'checkin_time',
                    'auto_travel', 'travel_time', 'auto_keepalive', 'keepalive_hours',
                    'auto_active', 'active_time', 'active_reports',
                    'wb2api_base', 'wb2api_key',
                    'default_weight', 'default_max_concurrency', 'default_min_interval',
                    'max_total_concurrency',
                    'retry_enabled', 'retry_attempts', 'retry_backoff')
        security.audit(actor, "update_pool_settings", "",
                       f"字段={'、'.join(sorted(k for k in body if k in editable))}；来源 {client_ip(req)}")
        return {"ok": True, "pool": settings}

    @app.get("/admin/api/pool/checkin-logs")
    async def checkin_logs(req: Request, limit: int = 200, kind: str = ""):
        """任务流水：签到 / 旅行 / 保活，新的在前。`kind` 为空时给全部。

        路径名保留 checkin-logs：它是最早的名字，绑定它的地方不少；改名的收益
        只是一个更好听的路径，代价是所有调用方都要跟着动。
        """
        viewer(req)
        return pool.task_logs(limit, kind or None)

    @app.get("/admin/api/request-logs")
    async def request_logs(req: Request, limit: int = 200):
        """每次 /v1 请求用了哪个账号、什么模型、扣了多少。

        与指标一样是内存环形缓冲：服务重启即清零，不写 state.json。
        """
        viewer(req)
        return pool.request_logs(limit)

    @app.get("/admin/api/usage/stats")
    async def usage_stats(req: Request, days: int = 30, hours: int = 24):
        """用量聚合（按天 / 按小时）：调用数、失败率、token、积分、耗时与首字延迟。

        与 request-logs 的分工：那边是「最近 300 条的明细」，这边是「长期的分桶计数」，
        所以这一份会落进 state.json（由池的 tick 每分钟兜底刷盘），重启不丢。

        返回里的 `throughput` 是按模型的输出速率排行，分母只算"真正在生成"的那段
        时间（总耗时 − 首字耗时），失败与 0 token 的请求不进样本。
        """
        viewer(req)
        return usage.snapshot(store, days=days, hours=hours)

    @app.get("/v1/usage")
    async def gateway_key_usage(req: Request):
        """网关密钥自查：已用 / 剩余额度、有效期、模型范围、IP 绑定情况。

        只读 —— 不查上游、不产生计费调用，token 与积分一分不记。
        但**要留下痕迹**：鉴权失败也记一条入站日志与 0 token 的请求日志，
        否则「谁在拿错密钥反复敲门」在后台完全看不见。
        """
        auth = req.headers.get("authorization") or ""
        x_api_key = req.headers.get("x-api-key") or ""
        token = auth[7:].strip() if auth.lower().startswith("bearer ") else ""
        token = token or x_api_key.strip()
        ip = client_ip(req)
        ua = (req.headers.get("user-agent") or "")[:120]

        def deny(message: str, reason: str):
            log_inbound(ip, "/v1/usage", reason, ua, blocked=True)
            pool.log_request(None, None, 401, False, 0, 0, 0, "rejected", "usage")
            return JSONResponse(
                {"error": {"message": message, "type": "auth_error", "code": "invalid_api_key"}}, 401)

        if not token:
            return deny("缺少 API Key", "missing")
        try:
            key = store.check_api(auth, x_api_key)
        except HTTPException:
            return deny("invalid api key", "invalid")
        log_inbound(ip, "/v1/usage", "", ua, blocked=False)
        # 0 token 的调用日志：只为留下「最近使用」与来源 IP，不计任何用量。
        pool.log_request(None, None, 200, True, 0, 0, 0, "success", "usage")

        if key is None:  # 临时测试密钥：无策略，直接告知
            return {"object": "usage", "key": None, "unrestricted": True,
                    "note": "临时测试密钥没有策略限制"}

        quota = int(key.get("quota") or 0)
        used_tokens = int(key.get("used_tokens") or 0)
        quota_credit = float(key.get("quota_credit") or 0)
        used_credit = round(float(key.get("used_credit") or 0), 6)
        return {
            "object": "usage",
            "key": {
                "id": key.get("id"),
                "name": key.get("name", ""),
                "prefix": key.get("prefix", ""),
                "hint": key.get("hint", ""),
                "enabled": bool(key.get("enabled", True)),
                "expires_at": key.get("expires_at"),
                "realm": keysvc.norm_realm(key.get("realm")),
                "models": list(key.get("models") or []),
                "created": key.get("created"),
                "last_used_at": key.get("last_used_at"),
            },
            "tokens": {
                "limit": quota,
                "used": used_tokens,
                # 0 表示不限；不限时不给 remaining，免得下游把 null 当成 0 处理
                "remaining": max(0, quota - used_tokens) if quota else None,
            },
            "credit": {
                "limit": quota_credit,
                "used": used_credit,
                "remaining": round(max(0.0, quota_credit - used_credit), 6) if quota_credit else None,
            },
            "ips": {
                "max": int(key.get("max_ips") or 0),
                "allowlist": list(key.get("ip_allowlist") or []),
                "bound": list((store.data.get("api_key_ips") or {}).get(key.get("id")) or []),
            },
        }

    @app.get("/admin/api/model-aliases")
    async def get_model_aliases(req: Request):
        """模型别名表 + 可用于校验的已知模型清单。"""
        viewer(req)
        known = converter.get_available_models()
        aliases = keysvc.model_aliases(store)
        return {
            "aliases": aliases,
            "known_models": known,
            "default_model": converter.resolve_default_model(),
            # 别名指向的模型不在清单里（多半是拼错，或本机还没下发这个模型）
            "unknown_targets": sorted({
                target for target in aliases.values()
                if known and target not in set(known)
            }),
        }

    @app.post("/admin/api/model-aliases")
    async def set_model_aliases(req: Request):
        """整表替换别名映射。传空表即清空。

        设计成整表替换而不是逐条增删：别名表很小（十几条），整表提交能避免
        「先删后加」中间态被读到的竞态，前端也好做「一个输入框一行」的编辑。
        """
        actor = admin(req)
        body = await payload(req)
        raw = body.get("aliases")
        if not isinstance(raw, dict):
            raise HTTPException(400, "aliases 必须是对象")
        cleaned: dict[str, str] = {}
        for key, value in raw.items():
            name, target = str(key).strip(), str(value).strip()
            if not name and not target:
                continue
            if not name or not target:
                raise HTTPException(400, "别名与目标模型都必须填写")
            if name == target:
                continue
            cleaned[name[:64]] = target[:64]
        if len(cleaned) > 200:
            raise HTTPException(400, "别名最多 200 条")
        with store.lock:
            store.data["model_aliases"] = cleaned
            store.save()
        security.audit(actor, "set_model_aliases", "", f"{len(cleaned)} 条；来源 {client_ip(req)}")
        return {"ok": True, "aliases": cleaned}

    @app.get("/admin/api/models")
    async def model_catalog(req: Request):
        """模型中心：本机可用模型的完整信息（上下文长度 / 最大输出 / 思考档位 / 积分单价）。

        数据源与 `/v1/models` 是同一份产品配置，只是不裁字段。额外补两样运营要看的东西：
          · `aliases`：哪些别名指向它——一眼看出「gpt-4o 实跑 glm-5.2」
          · `key_count`：有多少枚密钥的白名单里含它——决定某个模型能不能安全停用
        """
        viewer(req)
        # 带 free 标注：模型选择器的「只看限时免费」全靠它，判据只有 tags。
        catalog = converter.get_model_catalog_with_free()
        aliases = keysvc.model_aliases(store)
        reverse: dict[str, list[str]] = {}
        for alias, target in aliases.items():
            reverse.setdefault(target, []).append(alias)

        with store.lock:
            keys = list(store.data.get("keys", {}).values())

        for entry in catalog:
            mid = entry["id"]
            entry["aliases"] = sorted(reverse.get(mid, []))
            entry["key_count"] = sum(1 for item in keys if mid in (item.get("models") or []))

        source = converter._find_workbuddy_product_json()
        return {
            "models": catalog,
            "total": len(catalog),
            "free_count": sum(1 for entry in catalog if entry.get("free")),
            "default_model": converter.resolve_default_model(),
            "aliases": aliases,
            # 清单到底读的哪一份配置：产品配置有「装机快照」与「运行时缓存」两份，
            # 排「为什么看不到某个模型」时第一眼就要看这个。
            "source": str(source) if source else "",
        }

    @app.get("/admin/api/accounts/credits-snapshot")
    async def credits_snapshot(req: Request):
        """已登记积分的**只读快照**——给看板 / 对账脚本用。

        特点（与 `/admin/api/overview` 的区别）：**不查上游**。overview 会带上池的
        实时状态与签到信息；这里只是把 `account_status` 里上一次登记到的积分数
        原样摊平，所以它是可以高频轮询的。

        每行带 `known`：这个 uid 现在还**真实存在**于账号库吗？对账脚本靠它区分
        「还在的账号」与「只剩历史记录的 uid」——早期版本两者混在一起，
        对账时总分对不上却查不出原因。
        """
        viewer(req)
        rows = pool.rows(store.account_rows())
        out = []
        remaining_total = 0.0
        known_remaining = 0.0
        for row in rows:
            remaining = row.get("remaining")
            known = row.get("status") != "missing"
            credit = float(remaining) if isinstance(remaining, (int, float)) else None
            if credit is not None:
                remaining_total += credit
                if known:
                    known_remaining += credit
            out.append({
                "id": row.get("id"),
                "name": row.get("name"),
                "known": known,
                "enabled": bool(row.get("enabled", True)),
                "pool_state": row.get("pool_state"),
                "realm": row.get("realm"),
                "remaining": credit,
                "capacity": row.get("capacity"),
                "used": row.get("used"),
                "packages": row.get("packages"),
                "credits_updated": row.get("credits_updated"),
                "credits_stale": bool(row.get("credits_stale")),
                "today_checked_in": bool(row.get("today_checked_in")),
                "expires_at": row.get("expires_at"),
                "expired": bool(row.get("expired")),
                "request_count": row.get("request_count", 0),
            })
        return {
            "generated_at": int(time.time()),
            "rows": out,
            "known_count": sum(1 for r in out if r["known"]),
            "total_count": len(out),
            "totals": {
                "remaining": round(remaining_total, 6),
                # 被服务侧移除 / 账号库里已消失的 uid 不计入这一项
                "known_remaining": round(known_remaining, 6),
            },
        }

    @app.get("/admin/api/keys/{kid}/export")
    async def export_key(kid: str, req: Request):
        """把一枚密钥导出成下游客户端的配置片段（cc-switch / ZCode）。

        纯计算，**不写任何文件、不碰客户端配置**——返回值只交给前端展示/复制。

        反向的「导入」**有意没有实现**：上游 workbuddy-manager 的做法是结束
        用户正在运行的 cc-switch / ZCode 进程，再改写它们的 SQLite 库与
        `provider_config.json`。那是动别人软件的配置，风险与「读一份配置生成片段」
        完全不在一个量级，需要用户明确同意后才谈得上做。

        查询参数：`client`（ccswitch|zcode）、`app`（claude|codex，仅 ccswitch）、
        `base_url`（面板对外地址，默认取本机 127.0.0.1:端口）、`provider_id`、
        `name`。
        """
        actor = viewer(req)
        # ⚠️ 同一类泄露的第二处：导出功能产出的是**可直接用的配置**，
        # 里面必然含明文密钥。原先只要 viewer，等于只读身份也能把全站
        # 密钥导出来。导出本来就是"把密钥交出去"这个动作，归管理员。
        if str((actor or {}).get("role") or "") != "admin":
            raise HTTPException(403, "只有管理员可以导出密钥配置")
        client = (req.query_params.get("client") or "ccswitch").strip().lower()
        if client not in keyexport.CLIENTS:
            raise HTTPException(400, f"client 必须是 {keyexport.CLIENTS} 之一")
        with store.lock:
            item = store.data.get("keys", {}).get(kid)
            if not item:
                raise HTTPException(404, "密钥不存在")
            view = keysvc.public_key(kid, item)
            host = str(req.base_url).rstrip("/")
        try:
            result = keyexport.build(
                client=client,
                key=view,
                name=(req.query_params.get("name") or view.get("name") or "").strip(),
                base_url=(req.query_params.get("base_url") or host).strip(),
                app=(req.query_params.get("app") or "").strip().lower() or None,
                provider_id=(req.query_params.get("provider_id") or "").strip() or None,
                default_model=converter.resolve_default_model(),
                known_models=converter.get_available_models(),
                # 上下文长度来自模型中心，让 ZCode 那边能填上正确的窗口大小
                context_windows={m["id"]: m["context_window"]
                                 for m in converter.get_model_catalog()
                                 if m.get("context_window")},
            )
        except ValueError as exc:
            raise HTTPException(400, str(exc))
        result["key_id"] = kid
        # 导出含有明文密钥，属于敏感动作，记一笔（不记内容）
        security.audit(actor, "export_key", view.get("name") or kid,
                       f"client={client} app={result.get('app','')}；来源 {client_ip(req)}")
        return result

    # ── 上游分组 ──────────────────────────────────────────────
    # 一整套 CRUD 都用 `admin(req)`（读写都要权限）：分组的成员构成直接决定
    # 「哪个密钥能用哪些账号」，只读角色看得到就够了，改必须是有权限的人。

    @app.get("/admin/api/groups")
    async def list_groups(req: Request):
        """分组清单 + 可选成员账号清单（界面上的勾选框直接用）。"""
        viewer(req)
        rows = pool.rows(store.account_rows())
        return {
            "groups": groups.list_groups(store),
            "accounts": [{"id": row["id"], "name": row.get("name") or row["id"],
                          "pool_state": row.get("pool_state"),
                          "realm": row.get("realm")} for row in rows],
            "routings": list(groups.ROUTINGS),
        }

    @app.post("/admin/api/groups")
    async def create_group(req: Request):
        actor = admin(req)
        body = await payload(req)
        try:
            created = groups.create_group(
                store, body.get("name"),
                description=body.get("description") or "",
                accounts=body.get("accounts"),
                routing=body.get("routing") or "",
                enabled=body.get("enabled", True),
            )
        except ValueError as exc:
            raise HTTPException(400, str(exc))
        security.audit(actor, "create_group", created["name"], f"来源 {client_ip(req)}")
        return {"ok": True, **created, "groups": groups.list_groups(store)}

    @app.patch("/admin/api/groups/{gid}")
    async def update_group(gid: str, req: Request):
        actor = admin(req)
        body = await payload(req)
        try:
            groups.update_group(store, gid, body)
        except KeyError:
            raise HTTPException(404, "分组不存在")
        except ValueError as exc:
            raise HTTPException(400, str(exc))
        security.audit(actor, "update_group", gid,
                       f"字段={'、'.join(sorted(body))}；来源 {client_ip(req)}")
        return {"ok": True, "groups": groups.list_groups(store)}

    @app.delete("/admin/api/groups/{gid}")
    async def delete_group(gid: str, req: Request):
        """删除分组并解除密钥绑定。

        返回 `unbound_keys`：删完之后必须告诉用户「有 N 枚密钥从'限定在某组'
        变成了'不限账号'」——这是个**权限放大**的副作用，不能静默发生。
        """
        actor = admin(req)
        try:
            result = groups.delete_group(store, gid)
        except KeyError:
            raise HTTPException(404, "分组不存在")
        security.audit(actor, "delete_group", gid,
                       f"解绑 {result['unbound_keys']} 枚密钥；来源 {client_ip(req)}")
        return {"ok": True, **result, "groups": groups.list_groups(store)}

    # ── 账号出口线路 ──────────────────────────────────────────

    @app.get("/admin/api/egress")
    async def list_egress(req: Request):
        """线路清单 + 每个账号当前绑的线路。

        `url` 已经过 `egress.mask()`：带 `user:pass@` 的商用代理不能把口令
        原样送进浏览器（同一条线路往往多个下游共用）。
        """
        viewer(req)
        rows = pool.rows(store.account_rows())
        with store.lock:
            assign_map = dict((store.data.get("egress") or {}).get("assign") or {})
        return {
            "lines": egress.list_lines(store),
            "accounts": [{"id": row["id"], "name": row.get("name") or row["id"],
                          "pool_state": row.get("pool_state"),
                          "line_id": assign_map.get(row["id"]) or ""} for row in rows],
            # 两种「绑定不生效」分开给，因为处理方式不同：悬空引用必须重新指定线路，
            # 线路停用只要启用回来。混成一个列表，界面就只能写一句含糊的提示。
            "orphans": egress.orphans(store),
            "stale": egress.stale(store),
        }

    @app.post("/admin/api/egress/lines")
    async def create_egress_line(req: Request):
        actor = admin(req)
        body = await payload(req)
        try:
            created = egress.create_line(store, body.get("name"), body.get("url"),
                                         enabled=body.get("enabled", True))
        except ValueError as exc:
            raise HTTPException(400, str(exc))
        # 只记线路名与掩码后的地址：明文口令不进审计日志
        masked = egress.mask(egress.norm_url(body.get("url")))
        security.audit(actor, "create_egress_line", created["name"],
                       f"{masked}；来源 {client_ip(req)}")
        return {"ok": True, **created, "lines": egress.list_lines(store)}

    @app.patch("/admin/api/egress/lines/{lid}")
    async def update_egress_line(lid: str, req: Request):
        actor = admin(req)
        body = await payload(req)
        try:
            egress.update_line(store, lid, body)
        except KeyError:
            raise HTTPException(404, "线路不存在")
        except ValueError as exc:
            raise HTTPException(400, str(exc))
        security.audit(actor, "update_egress_line", lid,
                       f"字段={'、'.join(sorted(body))}；来源 {client_ip(req)}")
        return {"ok": True, "lines": egress.list_lines(store)}

    @app.delete("/admin/api/egress/lines/{lid}")
    async def delete_egress_line(lid: str, req: Request):
        actor = admin(req)
        try:
            freed = egress.delete_line(store, lid)
        except KeyError:
            raise HTTPException(404, "线路不存在")
        security.audit(actor, "delete_egress_line", lid, f"解绑 {freed} 个账号；来源 {client_ip(req)}")
        return {"ok": True, "freed": freed, "lines": egress.list_lines(store)}

    @app.post("/admin/api/egress/assign")
    async def assign_egress(req: Request):
        """给账号绑定 / 解绑出口线路：`{account_id, line_id}`，line_id 为空表示回直连。"""
        actor = admin(req)
        body = await payload(req)
        aid = str(body.get("account_id") or "").strip()
        lid = str(body.get("line_id") or "").strip()
        if not aid:
            raise HTTPException(400, "缺少 account_id")
        try:
            egress.assign(store, aid, lid or None)
        except KeyError as exc:
            raise HTTPException(404, "账号或线路不存在")
        security.audit(actor, "assign_egress", aid,
                       f"线路={lid or '直连'}；来源 {client_ip(req)}")
        return {"ok": True, "lines": egress.list_lines(store)}

    @app.get("/admin/api/pool/checkin-logs/summary")
    async def checkin_log_summary(req: Request, hours: int = 24):
        """任务流水的按类别汇总：每类成功/失败、最近一次时间、成功率。

        与 `/checkin-logs` 的分工：那边是明细（最多 200 条），这边是"今天到底
        签到成功几个号"这类一眼要能答的问题。
        """
        viewer(req)
        return pool.task_summary(hours)

    @app.post("/admin/api/pool/checkin-logs/clear")
    async def clear_checkin_logs(req: Request, kind: str = ""):
        actor = session_admin(req)
        # TASK_KINDS 是 admin.pool 的模块级常量，不是 AccountPool 实例的属性。
        # 这里必须用 import 进来的那个名字：`pool` 在本作用域里是实例（见 create_app
        # 开头的 `pool = AccountPool(store)`），写成 pool.TASK_KINDS 会 AttributeError → 500。
        if kind and kind not in TASK_KINDS:
            raise HTTPException(400, "任务类别无效")
        removed = pool.clear_task_logs(kind or None)
        security.audit(actor, "clear_checkin_logs", "",
                       f"清空 {removed} 条（类别={kind or '全部'}）；来源 {client_ip(req)}")
        return {"ok": True, "removed": removed}

    @app.post("/admin/api/accounts")
    async def add_account(req: Request):
        actor = admin(req)
        body = await payload(req)
        doc = body.get("credential")
        store.validate_credential(doc)
        name = clean_name(body.get("name"), None)
        # 统一走 save_browser_account：按 uid 去重合并（重登会刷新凭据），
        # 并让 SwitchStore 把新账号并入 workbuddy-switch 账号库。
        result = store.save_browser_account(doc, name)
        aid = result.get("account_id") or result.get("id")
        security.audit(actor, "add_account", str(name or aid), f"来源 {client_ip(req)}")
        return {"id": aid}

    @app.patch("/admin/api/accounts/{aid}")
    async def edit_account(aid: str, req: Request):
        actor = admin(req)
        body = await payload(req)
        live_accounts(store)
        hot = hasattr(store, "apply_override")
        with store.lock:
            item = store.data["accounts"].get(aid)
            if not item:
                raise HTTPException(404, "账号不存在")
            new = dict(item)
            if "name" in body:
                new["name"] = clean_name(body["name"], item["name"])
            if "enabled" in body:
                if type(body["enabled"]) is not bool:
                    raise HTTPException(400, "启用状态不正确")
                new["enabled"] = body["enabled"]
            if body.get("active") is True:
                if not new["enabled"]:
                    raise HTTPException(400, "请先启用账号")
                if hot:
                    # 账号库才是凭据的权威：这里只确认库里确实有这个账号的可用记录。
                    row = next((r for r in store.account_rows() if r["id"] == aid), None)
                    if row is None or row.get("status") != "ready":
                        raise HTTPException(400, "该账号在 switch 账号库里找不到凭据，请在 workbuddy-switch 中重新登录")
                else:
                    try:
                        store.validate_credential(json.loads(store.file_for(new).read_text(encoding="utf-8")))
                    except (ValueError, OSError):
                        raise HTTPException(400, "凭据文件无法读取，请重新导入")
                store.data["active"] = aid
                # 换「当前账号」时作废 spread 驻留租约，否则切了号还会被旧租约钉住。
                store.data["pool"]["lease"] = None
            store.data["accounts"][aid] = new
            if not new["enabled"] and store.data["active"] == aid:
                store.data["active"] = None
            store.save()
            store.sync_active()
        if hot:
            # 暂停 / 改名属于本服务的视图，落到覆盖层，否则下次账号库刷新就被抹掉。
            patch = {}
            if "enabled" in body:
                patch["enabled"] = body["enabled"]
            if "name" in body:
                patch["name"] = new["name"]
            if patch:
                store.apply_override(aid, **patch)
        security.audit(actor, "update_account", new["name"],
                       f"字段={'、'.join(sorted(k for k in body if k in ('name', 'enabled', 'active')))}；来源 {client_ip(req)}")
        return {"ok": True}

    @app.put("/admin/api/accounts/{aid}/limits")
    async def set_account_limits(aid: str, req: Request):
        """设置账号级流量整形：权重 / 并发上限 / 最小间隔。

        这三项是**防封号**的直接手段：封号的直接诱因几乎都是单账号短时间
        被打了太多请求。池级的 spread 是全域一刀切，改不了「这个新号先养着、
        那个稳的号多跑点」这种差别对待。

        传 `null`（JSON null）或干脆不传某个键 = 不动那一项，不是设成 0。
        这个区分很重要：界面上改权重时不该把并发上限一起清掉。
        """
        actor = admin(req)
        body = await payload(req)
        if not isinstance(body, dict):
            raise HTTPException(400, "参数格式不正确")
        values = {}
        for field, cap, label in (("weight", throttle.MAX_WEIGHT, "权重"),
                                  ("max_concurrency", throttle.MAX_CONCURRENCY, "并发上限"),
                                  ("min_interval", throttle.MAX_MIN_INTERVAL, "最小间隔")):
            if field not in body:
                continue
            raw = body[field]
            if raw is None:
                continue
            if isinstance(raw, bool):
                raise HTTPException(400, f"{label}必须是数字")
            try:
                num = float(raw)
            except (TypeError, ValueError):
                raise HTTPException(400, f"{label}必须是数字")
            if num != num or num in (float("inf"), float("-inf")):
                raise HTTPException(400, f"{label}必须是有限数字")
            if num < 0:
                raise HTTPException(400, f"{label}不能是负数")
            if num > cap:
                raise HTTPException(400, f"{label}最大 {cap:g}")
            values[field] = int(num) if field != "min_interval" else float(num)
        saved = pool.set_limits(aid, **values)
        security.audit(actor, "set_account_limits", aid,
                       f"字段={'、'.join(sorted(values)) or '无'}；来源 {client_ip(req)}")
        return {"ok": True, **saved}

    @app.delete("/admin/api/accounts/{aid}")
    async def delete_account(aid: str, req: Request):
        actor = session_admin(req)
        live_accounts(store)
        if hasattr(store, "hide_account"):
            # 账号库是权威来源，服务侧「删除」= 不再调度这个账号（本地覆盖），
            # 真删账号请到 workbuddy-switch 的账号管理页。
            with store.lock:
                if aid not in store.data["accounts"]:
                    raise HTTPException(404, "账号不存在")
            store.hide_account(aid)
            security.audit(actor, "hide_account", aid, f"仅从本服务移除，账号库保留；来源 {client_ip(req)}")
            return {"ok": True, "scope": "service"}
        with store.lock:
            item = store.data["accounts"].get(aid)
            if not item:
                raise HTTPException(404, "账号不存在")
            trash = store.root / "trash" / (aid + "-" + secrets.token_hex(4))
            trash.mkdir(parents=True, mode=0o700)
            source = store.file_for(item)
            if source.exists():
                write_json(trash / "credential.json", json.loads(source.read_text(encoding="utf-8")))
            write_json(trash / "metadata.json", item)
            del store.data["accounts"][aid]
            if store.data["active"] == aid:
                store.data["active"] = None
            store.save()
            store.sync_active()
            # 这一支是真删（账号库也随之作废），所以要顺手清掉分组与出口绑定，
            # 否则 state.json 里会留下永远匹配不到人的悬空 id。
            #
            # 上面 hide_account 那一支故意不清：隐藏是可逆的，清掉等于用户
            # 暂停一个号再改下分组，那个号的成员资格与线路就永久丢了。
            # 隐藏账号本来就进不了候选集，留着这两条配置没有任何副作用。
            groups.forget_account(store, aid)
            egress.unbind_account(store, aid)
            # Keep the file so an in-flight token refresh may still complete safely.
            # Account storage is authoritative; deleted records cannot be selected.
        security.audit(actor, "delete_account", aid, f"来源 {client_ip(req)}")
        return {"ok": True, "scope": "file"}

    @app.get("/admin/api/pool/summary")
    async def pool_summary(req: Request):
        """账号池概览：可直接喂给 SPA 的账号页。"""
        actor = viewer(req)
        # 与 overview 同口径：pool 段里有上游 Bearer 凭据，只给管理员。
        reveal = str((actor or {}).get("role") or "") == "admin"
        live_accounts(store)
        return {"accounts": pool.rows(store.account_rows()),
                "pool": _pool_view(store, reveal_secret=reveal)}

    # ── API 密钥（多密钥分发 + IP/模型白名单 + 额度）──────────────
    @app.get("/admin/api/cli/key")
    async def cli_key(req: Request):
        """取回（必要时创建）CLI 专用密钥。

        幂等：已有一枚可用的 CLI 密钥就原样返回，不会每次点「接入」都新发一枚。
        """
        actor = admin(req)
        kid, token, created = keysvc.ensure_cli_key(store, created_by=actor["username"])
        security.audit(actor, "create_key" if created else "read_key", "CodeBuddy CLI",
                       f"id={kid}；来源 {client_ip(req)}")
        return {"id": kid, "key": token, "created": created, "realm": "cn"}

    @app.post("/admin/api/cli/key/rotate")
    async def cli_key_rotate(req: Request):
        actor = admin(req)
        kid, token = keysvc.rotate_cli_key(store, created_by=actor["username"])
        security.audit(actor, "rotate_key", "CodeBuddy CLI",
                       f"id={kid}；来源 {client_ip(req)}")
        return {"id": kid, "key": token, "created": True, "realm": "cn"}

    @app.delete("/admin/api/cli/key")
    async def cli_key_revoke(req: Request):
        actor = admin(req)
        count = keysvc.revoke_cli_keys(store)
        security.audit(actor, "delete_key", "CodeBuddy CLI",
                       f"吊销 {count} 枚；来源 {client_ip(req)}")
        return {"ok": True, "revoked": count}

    @app.post("/admin/api/keys")
    async def add_key(req: Request):
        actor = admin(req)
        body = await payload(req)
        name = clean_name(body.get("name"), "新客户端")
        if not str(body.get("name") or "").strip():
            raise HTTPException(400, "密钥名称不能只有空格")
        ip_allowlist = body.get("ip_allowlist") or []
        for cidr in keysvc.norm_cidrs(ip_allowlist):
            if not valid_cidr(cidr):
                raise HTTPException(400, f"IP 白名单里有无法识别的条目：{cidr}。请写成单个 IP 或 CIDR（10.0.0.0/8）。")
        kid, key = keysvc.create_key(
            store, name,
            expires_at=body.get("expires_at"),
            max_ips=int(body.get("max_ips") or 0),
            ip_allowlist=ip_allowlist,
            models=body.get("models") or [],
            realm=body.get("realm") or "",
            quota=int(body.get("quota") or 0),
            quota_credit=body.get("quota_credit") or 0,
            # 分组绑定。`update_key` 是整包透传 `body` 所以本来就带得上，
            # 但这里是逐字段拆开传的——漏掉这一行会让「创建时勾了分组、保存后没生效」，
            # 而且只在重新打开列表时才发现。
            groups=body.get("groups") or [],
            created_by=actor["username"],
        )
        security.audit(actor, "create_key", name, f"id={kid}；来源 {client_ip(req)}")
        return {"id": kid, "key": key}

    @app.patch("/admin/api/keys/{kid}")
    async def edit_key(kid: str, req: Request):
        actor = admin(req)
        body = await payload(req)
        if "ip_allowlist" in body:
            for cidr in keysvc.norm_cidrs(body.get("ip_allowlist")):
                if not valid_cidr(cidr):
                    raise HTTPException(400, f"IP 白名单里有无法识别的条目：{cidr}。请写成单个 IP 或 CIDR（10.0.0.0/8）。")
        updated = keysvc.update_key(store, kid, body)
        security.audit(actor, "update_key", updated["name"], f"字段={'、'.join(sorted(body))}；来源 {client_ip(req)}")
        return updated

    @app.post("/admin/api/keys/{kid}/reset-usage")
    async def reset_key_usage(kid: str, req: Request):
        actor = admin(req)
        result = keysvc.reset_usage(store, kid)
        security.audit(actor, "reset_key_usage", kid, f"来源 {client_ip(req)}")
        return result

    @app.delete("/admin/api/keys/{kid}")
    async def revoke_key(kid: str, req: Request):
        actor = session_admin(req)
        result = keysvc.delete_key(store, kid)
        security.audit(actor, "delete_key", kid, f"来源 {client_ip(req)}")
        return result

    @app.post("/admin/api/keys/check-models")
    async def check_key_models(req: Request):
        """校验模型白名单里哪些名字匹配不到当前可用模型（拼写提醒）。"""
        viewer(req)
        body = await payload(req)
        names = [str(x).strip() for x in (body.get("models") or []) if str(x).strip()]
        if not names:
            return {"checked": True, "unknown": []}
        known = set(converter.get_available_models())
        unknown = keysvc.unknown_whitelist_entries(names, known)
        return {"checked": True, "unknown": unknown}

    @app.post("/admin/api/test")
    async def test_model(req: Request):
        admin(req)
        body = await payload(req)
        # 与数据面同一份默认模型（按本机可用清单解析），别在这里再写一个字面量。
        model = body.get("model", converter.resolve_default_model())
        prompt = body.get("prompt", "请只回复：连接成功")
        if not isinstance(model, str) or model not in converter.get_available_models():
            raise HTTPException(400, "请选择列表中的模型")
        if not isinstance(prompt, str) or not 1 <= len(prompt.strip()) <= 2000:
            raise HTTPException(400, "测试消息需为 1–2000 个字符")
        if store.test_lock.locked():
            raise HTTPException(409, "已有测试正在进行")
        if converter.CONFIG.get("cred") is None:
            raise HTTPException(400, "请先导入并启用一个账号")
        async with store.test_lock:
            started = time.monotonic()
            # Use a short-lived API key with the same authentication path as clients.
            with store.lock:
                temp_key = secrets.token_urlsafe(48)
                store.test_keys.add(digest(temp_key))
            try:
                async with httpx.AsyncClient(transport=httpx.ASGITransport(app=MetricsMiddleware(converter.app, metrics, source="test")), base_url="http://internal") as client:
                    result = await asyncio.wait_for(client.post("/v1/chat/completions", headers={"Authorization": "Bearer " + temp_key}, json={"model": model, "messages": [{"role": "user", "content": prompt}], "max_tokens": 1024, "stream": False}), timeout=90)
                data = result.json()
                answer = data.get("choices", [{}])[0].get("message", {}).get("content", "") if result.status_code == 200 else ""
                output = {"ok": result.status_code == 200 and bool(answer), "status": result.status_code, "answer": answer, "seconds": round(time.monotonic() - started, 2), "usage": data.get("usage") if result.status_code == 200 else None}
                if not output["ok"]:
                    output["error"] = "上游未返回有效回复，请检查登录凭据、账号额度或模型权限。"
                    if result.status_code == 200 and data.get("choices", [{}])[0].get("finish_reason") == "length":
                        output["error"] = "已连接上游，但生成预算耗尽，未获得正文。请在客户端提高 max_tokens 后重试。"
            except (Exception, asyncio.TimeoutError):
                output = {"ok": False, "error": "调用失败或超时，请检查凭据是否有效，稍后重试。", "seconds": round(time.monotonic() - started, 2)}
            finally:
                with store.lock:
                    store.test_keys.discard(digest(temp_key))
            store.events.appendleft({"time": int(time.time()), "model": model, "ok": output["ok"], "seconds": output["seconds"]})
            return output
    # ══ 服务设置 ══════════════════════════════════════════════
    #
    # 端点：GET/PUT /admin/api/settings。

    @app.get("/admin/api/settings")
    async def get_settings(req: Request):
        viewer(req)   # 只读角色也能看设置
        return settings_store.public_view(settings_store.load(store))

    @app.put("/admin/api/settings")
    async def put_settings(req: Request):
        # 改 SMTP 口令 / 开关注册 = 改服务的安全姿态，与 `/admin/api/security/config`
        # 同档，用 `session_admin`（拒 wbt_ 令牌身份，避免「令牌泄露 → 改 SMTP
        # 收走验证码」这条提权链）。
        actor = session_admin(req)
        body = await payload(req)
        saved = settings_store.apply_patch(store, body)
        # 审计绝不记录口令：这里只记「改了哪些键」。
        fields = "、".join(sorted(str(k) for k in body.keys()))
        security.audit(actor, "update_settings", "",
                       "字段=" + (fields or "(空)") + "；来源 " + client_ip(req))
        return saved


    # ── 服务设置结束 ──────────────────────────────────────────────

    # 端点：GET/PUT /admin/api/alerts —— 管理员告警通知。
    #
    # 只发信给管理员自己（积分/冻结/封禁），**不是用户系统**：
    # 没有注册、没有找回密码、没有验证码、没有面向终端用户的收件人。

    @app.get("/admin/api/alerts")
    async def get_alerts(req: Request):
        viewer(req)   # 只读角色也能看配置（口令只回掩码）
        return alerts.public_view(alerts.load(store))

    @app.put("/admin/api/alerts")
    async def put_alerts(req: Request):
        # 改 SMTP 口令 = 改服务的凭据，与 /admin/api/settings 同档，
        # 用 session_admin 拒 wbt_ 令牌身份。
        actor = session_admin(req)
        body = await payload(req)
        patch = alerts.normalize(body)
        if patch:
            with store.lock:
                current = store.data.get(alerts.SETTINGS_KEY)
                if not isinstance(current, dict):
                    current = {}
                current.update(patch)
                store.data[alerts.SETTINGS_KEY] = current
                store.save()
        # 审计绝不记录口令：只记「改了哪些键」。
        fields = "、".join(sorted(str(k) for k in body.keys()))
        security.audit(actor, "update_alerts", "",
                       "字段=" + (fields or "(空)") + "；来源 " + client_ip(req))
        return alerts.public_view(alerts.load(store))

    @app.post("/admin/api/alerts/test")
    async def test_alerts(req: Request):
        """发一封测试邮件，验证 SMTP 配置真的能通。

        这是配置类功能唯一可靠的自证方式 —— 否则管理员只能等下一次真告警
        才知道「原来没配通」，而那时候问题已经发生了。
        """
        session_admin(req)
        cfg = alerts.load(store)
        missing = [label for key, label in (("host", "SMTP 服务器"), ("to", "收件地址"))
                   if not cfg.get(key)]
        if missing:
            raise HTTPException(400, "请先填写：" + "、".join(missing))
        message = alerts.build_message(cfg, [{
            "kind": "test", "title": "测试邮件",
            "lines": ["这是一封测试邮件，收到即表示告警通道已配通。"],
        }])
        try:
            await asyncio.to_thread(alerts.send, cfg, message)
        except Exception as exc:  # noqa: BLE001
            # 把真实原因透给管理员：SMTP 报错文案（认证失败/连不上）本身就是
            # 排查所需的信息，包成「发送失败」等于让人去猜。
            raise HTTPException(400, f"发送失败：{type(exc).__name__}: {exc}")
        return {"ok": True, "message": f"测试邮件已发送至 {cfg.get('to')}"}


    # ── 服务设置结束 ──────────────────────────────────────────────

    # ═══════════════════════════════════════════════════════════

    # ⚠️ 本段必须注册在 `app.mount("/", converter.app)` 之前 ——
    # 那是兜底挂载，落在它后面的路由一律 404。


    def _int_param(raw, default: int) -> int:
        """query 参数一律是字符串，脏值吃成默认值 —— 不能让它变成 500。"""
        try:
            return int(str(raw).strip())
        except (TypeError, ValueError):
            return default


    app.mount("/", converter.app)
    return app


def _forwarded_allow_ips() -> str:
    """uvicorn 采信 `X-Forwarded-*` 的来源白名单。**默认只信本机**。

    为什么要收：`"*"` 表示「信任任何人写的转发头」。uvicorn 的 `proxy_headers`
    会用 `X-Forwarded-For` 覆盖 `scope["client"]`，于是**任何能直连到这个端口的
    人都能自称来自任意 IP**。

    ⚠️ 注意影响面**比看起来小**，别把它当万灵药也别当摆设：
      · `iputil.client_ip()` **不读这个开关** —— 它只看 TCP 对端是否落在
        `WB_TRUSTED_PROXY_CIDRS`，所以 IP 白名单 / 限流**不受**它影响；
      · 真正被它影响的是 **`_cookie_secure()` 读的 `X-Forwarded-Proto`**：
        伪造 `https` 会让 cookie 带上 `Secure`，而明文 HTTP 下浏览器拒绝回传
        → 表现为「登录成功但每个请求 401」。这是**可用性**问题，方向是「更严」。
      · 所以收紧它主要是**纵深防御**，不是堵一个已证实的洞。

    逃生门：反代在另一台机器 / 容器上时，它的 IP 不在 127.0.0.1，
    此时必须显式设置 `WB_FORWARDED_ALLOW_IPS`（逗号分隔），否则
    `X-Forwarded-Proto` 读不到 → 配了 HTTPS 反代的用户会「登录成功但一直 401」，
    且**极难自查**（没有任何报错）。部署文档里必须写这一条。
    """
    raw = os.environ.get("WB_FORWARDED_ALLOW_IPS", "").strip()
    if raw:
        values = [x.strip() for x in raw.split(",") if x.strip()]
        # 值可疑时打 warning：`*` 等于没设，而用户很可能以为设了就安全了。
        if "*" in values:
            print("[reverse-proxy] 警告：WB_FORWARDED_ALLOW_IPS 含 '*'，"
                  "等于信任任意来源的 X-Forwarded-* 头。请改为具体的反代地址。",
                  file=sys.stderr)
        return ",".join(values)
    return "127.0.0.1"


if __name__ == "__main__":
    # 默认绑 127.0.0.1：管理面只在回环上监听，
    # 绑 0.0.0.0 等于把它摊到所有网卡上。对外暴露请走外部反代
    # （反代终止 TLS → 转发到 127.0.0.1:8787），
    # 或显式传 `--host 0.0.0.0` 并同步设置 WB_FORWARDED_ALLOW_IPS。
    _host = os.environ.get("WB_SERVE_HOST", "").strip() or "127.0.0.1"
    if _host not in ("127.0.0.1", "localhost", "::1") and not os.environ.get("WB_FORWARDED_ALLOW_IPS", "").strip():
        print(f"[reverse-proxy] 警告：绑定在 {_host} 上（非本机回环），"
              "但 WB_FORWARDED_ALLOW_IPS 未设置 —— 若前方有反向代理，"
              "请把它的地址填进去，否则 HTTPS 反代下的登录态会失效。",
              file=sys.stderr)
    uvicorn.run(create_app(), host=_host, port=8787, log_level="warning",
                proxy_headers=True, forwarded_allow_ips=_forwarded_allow_ips())
