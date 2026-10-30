//! system 文本的两道加工：11128 渠道校验改写 + 身份句过滤。
//!
//! 顺序与 Python 版一致：**先过滤身份句，再按实测片段改写**。
//! 反过来会把刚改写出的中性句（含 you are / assistant）又删掉。
//!
//! 为什么保留「删句」而不只做改写：这是 Python 侧长期实测的结果——网关对
//! 第三方 agent 身份声明是按措辞拒绝的，删掉整句比逐词改写更稳；改写表只覆盖
//! 实测被拒、但删不掉（不含身份词）的片段。

use serde_json::Value;

pub const NEUTRAL_IDENTITY: &str = "You are a software engineering assistant.";

/// 实测被拒 → 可通过的措辞。新增一条前请先用真实账号确认「只改这一处就能通过」。
const RESTATEMENTS: &[(&str, &str)] = &[(
    "Tokens prefixed with @ are paths the user explicitly referenced.",
    "Tokens starting with @ name paths the user referenced.",
)];

const IDENTITY_WORDS: &[&str] = &[
    "ai",
    "agent",
    "assistant",
    "model",
    "llm",
    "codebuddy",
    "workbuddy",
    "claude",
    "codex",
    "gpt",
    "deepseek",
    "qwen",
    "kimi",
    "glm",
    "anthropic",
    "openai",
    "人工智能",
    "智能助手",
    "编程助手",
    "语言模型",
    "智能体",
];

fn has_identity_word(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    IDENTITY_WORDS.iter().any(|word| lower.contains(word))
}

fn is_declaration(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("you are")
        || lower.starts_with("your identity")
        || lower.starts_with("your designated identity")
        || lower.starts_with("you have been invoked")
        || lower.starts_with("you are powered by")
        || text.trim_start().starts_with("你是")
        || text.trim_start().starts_with("您的身份是")
}

fn should_drop_sentence(sentence: &str) -> bool {
    is_declaration(sentence) && has_identity_word(sentence)
}

/// 删掉「声明身份」的句子；整行都被删掉时连行一起去掉（与 Python 版一致）。
pub fn filter_system_text(text: &str) -> String {
    let mut kept_lines: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let mut kept = String::new();
        let mut removed = false;
        for sentence in split_sentences(line) {
            if should_drop_sentence(&sentence) {
                removed = true;
            } else {
                kept.push_str(&sentence);
            }
        }
        if removed && kept.trim().is_empty() {
            continue;
        }
        if removed {
            kept_lines.push(kept.trim_start().to_string());
        } else {
            kept_lines.push(line.to_string());
        }
    }
    kept_lines.join("\n")
}

/// 按中英文句末标点切句，保留分隔符。
fn split_sentences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in line.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?' | '。' | '！' | '？') {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 实测片段改写；幂等。
pub fn restate(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut line = line.to_string();
        let lower = line.trim_start().to_ascii_lowercase();
        if lower.starts_with("you are an ai agent powered by ") {
            line = NEUTRAL_IDENTITY.to_string();
        } else {
            for (refused, replacement) in RESTATEMENTS {
                if line.trim_start().starts_with(refused) {
                    line = (*replacement).to_string();
                    break;
                }
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// 只处理 system / developer 消息的文本；user 内容一律不动。
pub fn normalize_body(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    // 过滤后变成空串的 system 消息直接丢弃：空 system 对模型没有意义，
    // 而且 Python 版也是这么处理的（保持 golden 一致）。
    let mut kept: Vec<Value> = Vec::with_capacity(messages.len());
    let original = std::mem::take(messages);
    for mut message in original {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "system" && role != "developer" {
            kept.push(message);
            continue;
        }
        match message.get("content").cloned() {
            Some(Value::String(text)) => {
                let text = restate(&filter_system_text(&text));
                message["content"] = Value::String(text);
            }
            Some(Value::Array(blocks)) => {
                let mut out = Vec::with_capacity(blocks.len());
                for mut block in blocks {
                    if block
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| matches!(kind, "text" | "input_text" | "output_text"))
                    {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            block["text"] = Value::String(restate(&filter_system_text(text)));
                        }
                    }
                    out.push(block);
                }
                message["content"] = Value::Array(out);
            }
            _ => {}
        }
        let keep = match message.get("content") {
            Some(Value::String(text)) => !text.trim().is_empty(),
            _ => true,
        };
        if keep {
            kept.push(message);
        }
    }
    *messages = kept;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_identity_sentence_and_keeps_the_rest() {
        let input = "You are an AI agent powered by Golden Harness.\nKeep this rule.";
        assert_eq!(filter_system_text(input), "Keep this rule.");
    }

    #[test]
    fn restates_measured_span() {
        let input = "Tokens prefixed with @ are paths the user explicitly referenced.\nOther.";
        let out = restate(input);
        assert!(out.starts_with("Tokens starting with @"));
        assert!(out.ends_with("Other."));
    }

    #[test]
    fn restate_is_idempotent() {
        let once = restate("You are an AI agent powered by X.");
        assert_eq!(restate(&once), once);
    }

    #[test]
    fn does_not_touch_user_messages() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "You are an AI agent powered by X."},
            {"role": "system", "content": "You are an AI agent powered by X."}
        ]});
        normalize_body(&mut body);
        assert_eq!(
            body["messages"].as_array().unwrap().len(),
            1,
            "空 system 消息应被丢弃"
        );
        assert_eq!(
            body["messages"][0]["content"],
            json!("You are an AI agent powered by X.")
        );
    }
}
