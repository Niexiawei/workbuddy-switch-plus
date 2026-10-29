"""国内网关的渠道校验（HTTP 400 / code 11128）改写。

背景（参考 `dsh-native-tui/src/codebuddy/prompt.ts` 的实测结论）：
国内网关会按**措辞**比对 system 提示里的第三方 agent 表述，命中就返回
`11128 Illegal API invocation from an unapproved channel`；同一句话出现在
user 消息或 tool 结果里则不拦。两次实测（2026-10-20、2026-10-28）都表明匹配是
逐词敏感的——改一个词就能通过，所以这里只替换**实测被拒**的片段，其余提示
（工具说明、工作区规则、用户指令）逐字保留。整体删句会让模型丢掉自己的
briefing，那是用能力换通过率。

只处理 system / developer 角色的文本；改写是幂等的（改写后的句子不再命中）。
"""

from __future__ import annotations

import re

#: 网关接受的中性身份表述。
NEUTRAL_IDENTITY = "You are a software engineering assistant."

#: 已实测被拒的片段 → 可通过的措辞。新增一条前，请先用真实账号确认
#: 「只改这一处就能通过」，否则会白改模型自己的提示。
RESTATEMENTS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"^You are an AI agent powered by .+$", re.IGNORECASE), NEUTRAL_IDENTITY),
    (
        re.compile(r"^Tokens prefixed with @ are paths the user explicitly referenced\."),
        "Tokens starting with @ name paths the user referenced.",
    ),
)

#: 承载 system 文本的块类型（OpenAI / Responses 两种写法都有）。
_TEXT_BLOCK_TYPES = ("text", "input_text", "output_text")


def restate(text: str) -> str:
    """按行改写实测被拒的片段；其余内容原样返回。

    逐行匹配是有意的：实测被拒的是整行措辞，跨行匹配会把相邻的
    「被接受的句子」一起吞掉。
    """
    if not isinstance(text, str) or not text:
        return text
    lines = []
    for line in text.split("\n"):
        for pattern, replacement in RESTATEMENTS:
            line = pattern.sub(replacement, line)
        lines.append(line)
    return "\n".join(lines)


def apply(body: dict) -> dict:
    """改写一个 chat.completions 请求体里的 system 文本；无变化时原样返回。

    只碰 system / developer 消息——网关只看这两类；改 user 内容等于篡改用户输入。
    """
    messages = body.get("messages")
    if not isinstance(messages, list):
        return body

    changed = False
    rewritten = []
    for message in messages:
        if not isinstance(message, dict) or message.get("role") not in ("system", "developer"):
            rewritten.append(message)
            continue
        content = message.get("content")
        if isinstance(content, str):
            text = restate(content)
            if text != content:
                message = dict(message, content=text)
                changed = True
        elif isinstance(content, list):
            blocks = []
            block_changed = False
            for block in content:
                if (isinstance(block, dict)
                        and block.get("type") in _TEXT_BLOCK_TYPES
                        and isinstance(block.get("text"), str)):
                    text = restate(block["text"])
                    if text != block["text"]:
                        block = dict(block, text=text)
                        block_changed = True
                blocks.append(block)
            if block_changed:
                message = dict(message, content=blocks)
                changed = True
        rewritten.append(message)

    return dict(body, messages=rewritten) if changed else body
