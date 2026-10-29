"""11128 渠道校验：只改写实测被拒的片段，其余提示逐字保留。"""

from core import cn_prompt


def test_restates_agent_identity_line():
    text = "You are an AI agent powered by DeepSeek Harness v3.\nKeep this line."
    out = cn_prompt.restate(text)
    assert out.splitlines()[0] == cn_prompt.NEUTRAL_IDENTITY
    assert out.splitlines()[1] == "Keep this line."


def test_restates_at_token_sentence():
    text = "Tokens prefixed with @ are paths the user explicitly referenced."
    assert cn_prompt.restate(text) == "Tokens starting with @ name paths the user referenced."


def test_is_idempotent():
    once = cn_prompt.restate("You are an AI agent powered by X.")
    assert cn_prompt.restate(once) == once


def test_leaves_unmeasured_text_alone():
    text = "You are CodeBuddy Code.\n\nUse the tools carefully.\n@src/main.ts is a path."
    assert cn_prompt.restate(text) == text


def test_apply_only_touches_system_and_developer():
    body = {
        "messages": [
            {"role": "system", "content": "You are an AI agent powered by X."},
            {"role": "developer", "content": [{"type": "text", "text": "Tokens prefixed with @ are paths the user explicitly referenced."}]},
            {"role": "user", "content": "You are an AI agent powered by X."},
        ]
    }
    out = cn_prompt.apply(body)
    assert out["messages"][0]["content"] == cn_prompt.NEUTRAL_IDENTITY
    assert out["messages"][1]["content"][0]["text"].startswith("Tokens starting with @")
    assert out["messages"][2]["content"] == "You are an AI agent powered by X."  # 用户输入不动


def test_apply_returns_same_object_when_untouched():
    body = {"messages": [{"role": "user", "content": "hi"}]}
    assert cn_prompt.apply(body) is body
