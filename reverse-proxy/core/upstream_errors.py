"""上游失败分类：HTTP 状态 + 业务码 → 失败类型 / 客户端状态码 / 冷却维度。

为什么要单独一层：网关的 `code` 决定「换号有没有用」，而 HTTP 状态只说明
「这次请求失败了」。`11102` 是模型维度（换号无用）、`11134` 是限流（换号有用）、
`11128` 是请求形状（换号无用）——三者都是 4xx，但处置完全不同。

分类只认状态码与业务码，**不按 msg 文案判断**：同一句文案既可能是账号级失败，
也可能是请求级失败（参考项目 `errors.ts` 的同一条结论）。
"""

from __future__ import annotations

import re
from dataclasses import dataclass

# ── 失败类型 ──────────────────────────────────────────────────────────────
AUTH = "auth"                    # 凭据被拒：刷新/换号有意义
QUOTA = "quota"                  # 额度耗尽：换号有意义
RATE_LIMIT = "rate_limit"        # 限流：退避或换号
NO_SUCH_MODEL = "no_such_model"  # 模型不可用：换号无用，等目录刷新
BAD_REQUEST = "bad_request"      # 请求形状 / 渠道校验：换号无用
UNAVAILABLE = "unavailable"      # 上游故障：可换号重试
INCOMPLETE_STREAM = "incomplete_stream"
STREAM_TIMEOUT = "stream_timeout"
EMPTY_RESPONSE = "empty_response"
UNKNOWN = "unknown"

#: 网关业务码 → 失败类型（只收已确认含义的码）。
GATEWAY_CODES: dict[int, str] = {
    11102: NO_SUCH_MODEL,
    11103: BAD_REQUEST,
    11128: BAD_REQUEST,
    11133: BAD_REQUEST,
    11134: RATE_LIMIT,
    11135: BAD_REQUEST,
}

#: 渠道校验失败的可操作提示（与参考项目 `errors.ts` 同义）。
HINTS = {
    11128: "CodeBuddy 拒绝了这次调用（渠道校验 11128）。换号或重试都没用，"
           "要改的是发出去的 system 提示措辞：见 core/cn_prompt.py 的 RESTATEMENTS。",
    11102: "该账号没有这个模型。换号无用，请换模型或等目录刷新。",
}


@dataclass(frozen=True)
class Failure:
    """一次上游失败的完整处置口径。"""

    kind: str
    code: int | None
    message: str
    http_status: int
    #: 账号级冷却秒数（0 = 不冷却）。
    account_cooldown: int = 0
    #: 模型级冷却秒数（只对「该账号 + 该模型」生效，0 = 不冷却）。
    model_cooldown: int = 0
    #: 换另一个账号重放是否有意义。
    retryable: bool = False
    hint: str | None = None


_SECRET = re.compile(r"\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]{8,}", re.IGNORECASE)


def redact(text: str) -> str:
    return _SECRET.sub(r"\1 <redacted>", text or "")


def _message_of(body: dict) -> str:
    for key in ("msg", "message"):
        value = body.get(key)
        if isinstance(value, str) and value.strip():
            return redact(value.strip())
    error = body.get("error")
    if isinstance(error, dict):
        value = error.get("message")
        if isinstance(value, str) and value.strip():
            return redact(value.strip())
    elif isinstance(error, str) and error.strip():
        return redact(error.strip())
    return ""


def _cooldown_for(kind: str, code: int | None = None) -> tuple[int, int, bool]:
    """失败类型 → (账号冷却, 模型冷却, 可换号重试)。

    限流要分两种：业务码 `11134` 是**模型级**（同号换模型还能用），
    而裸 HTTP 429 是账号级（沿用存量行为：冷却 30 分钟）。
    """
    if kind == AUTH:
        return 300, 0, True
    if kind == QUOTA:
        return 1800, 0, True
    if kind == RATE_LIMIT:
        return (0, 300, True) if code == 11134 else (1800, 0, True)
    if kind == NO_SUCH_MODEL:
        return 0, 1800, False
    if kind == BAD_REQUEST:
        return 0, 0, False
    if kind == UNAVAILABLE:
        return 60, 0, True
    if kind in (INCOMPLETE_STREAM, STREAM_TIMEOUT):
        return 60, 0, True
    if kind == EMPTY_RESPONSE:
        return 0, 0, True
    return 0, 0, False


def _kind_of_code(code: int | None) -> str | None:
    return GATEWAY_CODES.get(code) if isinstance(code, int) else None


def _kind_of_status(status: int) -> str:
    if status in (401, 403):
        return AUTH
    if status == 402:
        return QUOTA
    if status == 429:
        return RATE_LIMIT
    if status in (404, 405):
        return UNAVAILABLE
    if status >= 500:
        return UNAVAILABLE
    if status >= 400:
        return BAD_REQUEST
    return UNKNOWN


def _http_status_for(kind: str, raw_status: int) -> int:
    if kind == AUTH:
        return 401
    if kind in (QUOTA, RATE_LIMIT):
        return 429
    if kind in (NO_SUCH_MODEL,):
        return 404
    if kind == BAD_REQUEST:
        return 400
    if kind in (UNAVAILABLE, INCOMPLETE_STREAM, STREAM_TIMEOUT, EMPTY_RESPONSE):
        return 502
    return raw_status if 400 <= raw_status <= 599 else 502


def classify(status: int, text: str = "", *, code: int | None = None,
             error_type: str | None = None, message: str | None = None) -> Failure:
    """把一个失败响应归类。

    参数按「知道多少给多少」：能拿到业务码就用业务码，否则退回 HTTP 状态；
    `error_type` 是流内错误帧自报的类型（`incomplete_stream` 等），优先级最高。
    """
    body: dict = {}
    if text:
        try:
            import json
            parsed = json.loads(text)
            if isinstance(parsed, dict):
                body = parsed
        except Exception:  # noqa: BLE001 - 非 JSON 就按状态判断
            body = {}
    if code is None and isinstance(body.get("code"), int):
        code = body["code"]

    if error_type in (INCOMPLETE_STREAM, STREAM_TIMEOUT, EMPTY_RESPONSE):
        kind = error_type
    else:
        kind = _kind_of_code(code) or _kind_of_status(status)

    detail = message or _message_of(body) or redact((text or "")[:300]).strip()
    if not detail:
        detail = f"upstream returned HTTP {status}" if status else "upstream failure"

    account_cooldown, model_cooldown, retryable = _cooldown_for(kind, code)
    return Failure(
        kind=kind,
        code=code,
        message=detail,
        http_status=_http_status_for(kind, status),
        account_cooldown=account_cooldown,
        model_cooldown=model_cooldown,
        retryable=retryable,
        hint=HINTS.get(code) if isinstance(code, int) else None,
    )


def error_payload(failure: Failure) -> dict:
    """OpenAI 兼容的错误体；`code` 保留上游业务码，便于客户端与日志对得上。"""
    error = {
        "message": failure.message + (("\n\n" + failure.hint) if failure.hint else ""),
        "type": failure.kind,
        "code": failure.code if failure.code is not None else failure.http_status,
        "wb_kind": failure.kind,
    }
    return {"error": error}


def sse_error_frame(failure: Failure) -> bytes:
    """一行 SSE 错误帧（流已经开始、无法再改 HTTP 状态时使用）。"""
    import json
    return ("data: " + json.dumps(error_payload(failure), ensure_ascii=False) + "\n\n").encode()
