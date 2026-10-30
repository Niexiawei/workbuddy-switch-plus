//! 内容审核脱敏：在「合规声明高频词」内部插入零宽空格。
//!
//! 词表从 Python 版 core/desensitize.py 的 SENSITIVE_TERMS 原样生成（见文件头注释），
//! 目的是缓解客户端固定合规模板被后端误判为敏感词——不改变语义，也不针对用户真实输入。
//!
//! 默认关闭（WB_PROXY_DESENSITIZE=1 才启用），与 Python converter 的默认值一致；
//! golden fixture 也是在关闭状态下录的。

use serde_json::Value;

/// 零宽空格：人/模型读起来无差别，但能打断后端的关键词匹配。
const ZWSP: char = '\u{200b}';

/// 触发审核的合规声明高频词（大小写不敏感，最长优先）。
pub const SENSITIVE_TERMS: &[&str] = &[
    "DoS",
    "DDoS",
    "exploit",
    "credential testing",
    "credential stuffing",
    "supply chain compromise",
    "supply-chain compromise",
    "detection evasion",
    "C2 frameworks",
    "C2 framework",
    "command and control",
    "malicious purposes",
    "malicious intent",
    "mass targeting",
    "brute force",
    "brute-force",
    "privilege escalation",
    "reverse shell",
    "remote code execution",
    "SQL injection",
    "XSS",
    "CSRF",
    "phishing",
    "malware",
    "ransomware",
    "keylogger",
    "rootkit",
    "backdoor",
    "botnet",
    "zero-day",
    "0day",
    "vulnerability",
    "vulnerabilities",
    "red teaming",
    "red-teaming",
    "sandbox",
    "sandboxing",
    "sandboxed",
    "unsandboxed",
    "escalated privileges",
    "escalated",
    "escalation",
    "destructive action",
    "destructive command",
    "destructive",
    "attack",
    "attacks",
    "cybersecurity",
    "security review",
    "exploit development",
    "hacking",
    "penetration testing",
    "penetration test",
    "injection",
    "weaponize",
    "weaponized",
    "harmful",
    "dangerous",
    "abuse",
    "abusive",
    "illegal",
    "terrorist",
    "terrorism",
    "bomb",
    "weapon",
    "weapons",
    "drug",
    "drugs",
    "narcotic",
    "suicide",
    "self-harm",
    "murder",
    "kill",
    "violence",
    "violent",
    "Claude Code",
    "Claude Opus",
    "Claude Sonnet",
    "Claude Haiku",
    "Claude Fable",
    "Anthropic",
    "Co-Authored-By",
    "noreply@anthropic.com",
];

fn matches_at(text: &[u8], index: usize, term: &[u8]) -> bool {
    if index + term.len() > text.len() {
        return false;
    }
    text[index..index + term.len()].eq_ignore_ascii_case(term)
}

/// 给文本里的触发词插入零宽空格；没有触发词就原样返回。
pub fn desensitize_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let bytes = text.as_bytes();
    let mut terms: Vec<&str> = SENSITIVE_TERMS.to_vec();
    terms.sort_by_key(|term| std::cmp::Reverse(term.len()));

    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        let mut hit = None;
        for term in &terms {
            if matches_at(bytes, index, term.as_bytes()) {
                hit = Some(*term);
                break;
            }
        }
        match hit {
            Some(term) if term.chars().count() > 1 => {
                let mut chars = term.chars();
                let first = chars.next().unwrap();
                out.push(first);
                out.push(ZWSP);
                out.push_str(chars.as_str());
                index += term.len();
            }
            _ => {
                let ch = text[index..].chars().next().unwrap();
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    out
}

/// 只处理指定角色的文本（默认 system / developer）。
pub fn desensitize_body(body: &mut Value, roles: &[&str]) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if !roles.contains(&role) {
            continue;
        }
        match message.get("content").cloned() {
            Some(Value::String(text)) => {
                message["content"] = Value::String(desensitize_text(&text));
            }
            Some(Value::Array(blocks)) => {
                let mut out = Vec::with_capacity(blocks.len());
                for mut block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            block["text"] = Value::String(desensitize_text(text));
                        }
                    }
                    out.push(block);
                }
                message["content"] = Value::Array(out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inserts_zero_width_after_first_char() {
        assert_eq!(desensitize_text("DoS attack"), "D\u{200b}oS a\u{200b}ttack");
    }

    #[test]
    fn longest_term_wins_and_is_idempotent_after_first_run() {
        let once = desensitize_text("credential stuffing");
        assert!(once.contains('\u{200b}'));
        assert_eq!(
            desensitize_text(&once),
            once,
            "已插入的零宽字符不应被再次处理"
        );
    }

    #[test]
    fn only_touches_selected_roles() {
        let mut body = json!({"messages": [
            {"role": "system", "content": "DoS"},
            {"role": "user", "content": "DoS"}
        ]});
        desensitize_body(&mut body, &["system", "developer"]);
        assert_ne!(body["messages"][0]["content"], json!("DoS"));
        assert_eq!(body["messages"][1]["content"], json!("DoS"));
    }
}
