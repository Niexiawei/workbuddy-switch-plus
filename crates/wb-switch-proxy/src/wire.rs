//! 请求体规范化：字段白名单 + developer→system + 身份过滤 + 11128 改写。
//!
//! 与 Python 版 \`converter.chat_completions\` 的前半段逐条对应；
//! 用 \`tests/fixtures/proxy/dataplane/chat_*.json\` 做 golden 对齐。

use serde_json::{json, Map, Value};

use crate::profile;
use crate::prompt;

/// 没指定 model 时代替客户端选的默认值（与 Python 版一致）。
pub const DEFAULT_MODEL: &str = "deepseek-v4.1-flash";

/// 透传字段白名单。不在表里的字段一律丢弃，避免把客户端私有字段打到上游。
pub const PASSTHROUGH_BODY_KEYS: &[&str] = &[
    "model", "messages", "tools", "tool_choice", "temperature", "max_tokens",
    "max_completion_tokens", "top_p", "stream", "stream_options", "stop",
    "presence_penalty", "frequency_penalty", "n", "response_format", "seed", "user",
    "reasoning_effort", "verbosity", "reasoning_summary",
];

pub fn normalize_chat(payload: &Value, profile: &str) -> Value {
    let Some(source) = payload.as_object() else {
        return payload.clone();
    };
    let mut body = Map::new();
    for key in PASSTHROUGH_BODY_KEYS {
        if let Some(value) = source.get(*key) {
            body.insert((*key).to_string(), value.clone());
        }
    }
    if profile == profile::CLI {
        for key in profile::CLI_EXTRA_BODY_KEYS {
            if let Some(value) = source.get(*key) {
                body.insert((*key).to_string(), value.clone());
            }
        }
    }
    if !body.contains_key("model") {
        body.insert("model".to_string(), json!(DEFAULT_MODEL));
    }
    // 上游只吃流式：非流式客户端由下游聚合。
    body.insert("stream".to_string(), json!(true));
    body.entry("stream_options".to_string())
        .or_insert_with(|| json!({"include_usage": true}));

    // developer 角色会触发上游安全策略，统一映射为 system 后再做文本加工。
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut() {
            if message.get("role").and_then(Value::as_str) == Some("developer") {
                message["role"] = json!("system");
            }
        }
    }

    let mut value = Value::Object(body);
    prompt::normalize_body(&mut value);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/proxy/dataplane")
            .join(format!("{name}.json"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn assert_golden(name: &str, expected_profile: &str) {
        let case = fixture(name);
        let headers = profile::lower_headers(
            case["request"]["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str().unwrap_or(""))),
        );
        let detected = profile::detect(&headers);
        assert_eq!(detected, expected_profile, "{name} 的 profile 判定不一致");

        let actual = normalize_chat(&case["request"]["body"], detected);
        let expected: Value =
            serde_json::from_str(case["upstream"]["body"].as_str().unwrap()).unwrap();
        assert_eq!(actual, expected, "{name} 的上游请求体与 Python 不一致");
    }

    #[test]
    fn chat_cli_matches_python_golden() {
        assert_golden("chat_cli", profile::CLI);
    }

    #[test]
    fn chat_desktop_matches_python_golden() {
        assert_golden("chat_desktop", profile::DESKTOP);
    }

    #[test]
    fn unknown_fields_are_dropped_and_defaults_added() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "store": true});
        let out = normalize_chat(&body, profile::DESKTOP);
        assert!(out.get("store").is_none());
        assert_eq!(out["model"], json!(DEFAULT_MODEL));
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["stream_options"]["include_usage"], json!(true));
    }
}
