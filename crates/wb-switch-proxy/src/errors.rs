//! 上游失败分类：业务码 + HTTP 状态 → 客户端状态码与冷却维度。
//!
//! 只认码不认文案：同一句 msg 既可能是账号级也可能是请求级失败。
//! 与 Python 版 `core/upstream_errors.py` 保持同一张表。

use serde_json::{json, Value};

pub const AUTH: &str = "auth";
pub const QUOTA: &str = "quota";
pub const RATE_LIMIT: &str = "rate_limit";
pub const NO_SUCH_MODEL: &str = "no_such_model";
pub const BAD_REQUEST: &str = "bad_request";
pub const UNAVAILABLE: &str = "unavailable";
pub const INCOMPLETE_STREAM: &str = "incomplete_stream";
pub const STREAM_TIMEOUT: &str = "stream_timeout";
pub const EMPTY_RESPONSE: &str = "empty_response";
pub const UNKNOWN: &str = "unknown";

#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub kind: &'static str,
    pub code: Option<i64>,
    pub message: String,
    pub http_status: u16,
    pub account_cooldown: u64,
    pub model_cooldown: u64,
    pub retryable: bool,
    pub hint: Option<&'static str>,
}

fn kind_of_code(code: Option<i64>) -> Option<&'static str> {
    match code {
        Some(11102) => Some(NO_SUCH_MODEL),
        Some(11103) | Some(11128) | Some(11133) | Some(11135) => Some(BAD_REQUEST),
        Some(11134) => Some(RATE_LIMIT),
        _ => None,
    }
}

fn kind_of_status(status: u16) -> &'static str {
    match status {
        401 | 403 => AUTH,
        402 => QUOTA,
        429 => RATE_LIMIT,
        404 | 405 => UNAVAILABLE,
        status if status >= 500 => UNAVAILABLE,
        status if status >= 400 => BAD_REQUEST,
        _ => UNKNOWN,
    }
}

fn cooldown_for(kind: &str, code: Option<i64>) -> (u64, u64, bool) {
    match kind {
        AUTH => (300, 0, true),
        QUOTA => (1800, 0, true),
        RATE_LIMIT if code == Some(11134) => (0, 300, true),
        RATE_LIMIT => (1800, 0, true),
        NO_SUCH_MODEL => (0, 1800, false),
        BAD_REQUEST => (0, 0, false),
        UNAVAILABLE | INCOMPLETE_STREAM | STREAM_TIMEOUT => (60, 0, true),
        _ => (0, 0, false),
    }
}

fn http_status_for(kind: &str, raw: u16) -> u16 {
    match kind {
        AUTH => 401,
        QUOTA | RATE_LIMIT => 429,
        NO_SUCH_MODEL => 404,
        BAD_REQUEST => 400,
        UNAVAILABLE | INCOMPLETE_STREAM | STREAM_TIMEOUT | EMPTY_RESPONSE => 502,
        _ if (400..=599).contains(&raw) => raw,
        _ => 502,
    }
}

/// 从错误体里取业务码与可读消息（容错：非 JSON 用原文前 300 字）。
pub fn classify(status: u16, body: &str, error_type: Option<&str>) -> Failure {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|value| value.get("code"))
        .and_then(Value::as_i64);
    let message = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("msg")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect::<String>());
    let kind = match error_type {
        Some(INCOMPLETE_STREAM) => INCOMPLETE_STREAM,
        Some(STREAM_TIMEOUT) => STREAM_TIMEOUT,
        Some(EMPTY_RESPONSE) => EMPTY_RESPONSE,
        _ => kind_of_code(code).unwrap_or_else(|| kind_of_status(status)),
    };
    let (account_cooldown, model_cooldown, retryable) = cooldown_for(kind, code);
    Failure {
        kind,
        code,
        message: if message.trim().is_empty() { format!("upstream returned HTTP {status}") } else { message },
        http_status: http_status_for(kind, status),
        account_cooldown,
        model_cooldown,
        retryable,
        hint: match code {
            Some(11128) => Some("CodeBuddy 拒绝了这次调用（渠道校验 11128）。换号或重试都没用，需要改 system 提示措辞。"),
            Some(11102) => Some("该账号没有这个模型。换号无用，请换模型或等目录刷新。"),
            _ => None,
        },
    }
}

pub fn error_payload(failure: &Failure) -> Value {
    let message = match failure.hint {
        Some(hint) => format!("{}\n\n{}", failure.message, hint),
        None => failure.message.clone(),
    };
    json!({"error": {
        "message": message,
        "type": failure.kind,
        "code": failure.code.unwrap_or(failure.http_status as i64),
        "wb_kind": failure.kind,
    }})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_check_is_not_retryable() {
        let failure = classify(400, r#"{"code":11128,"msg":"Illegal"}"#, None);
        assert_eq!(failure.kind, BAD_REQUEST);
        assert_eq!(failure.http_status, 400);
        assert!(!failure.retryable);
        assert!(failure.hint.unwrap().contains("11128"));
    }

    #[test]
    fn model_rate_limit_cools_only_model() {
        let failure = classify(400, r#"{"code":11134}"#, None);
        assert_eq!(failure.kind, RATE_LIMIT);
        assert_eq!(failure.model_cooldown, 300);
        assert_eq!(failure.account_cooldown, 0);
    }

    #[test]
    fn bare_429_cools_account() {
        let failure = classify(429, "", None);
        assert_eq!(failure.account_cooldown, 1800);
        assert_eq!(failure.model_cooldown, 0);
    }

    #[test]
    fn unknown_body_still_reports_message() {
        let failure = classify(503, "<html>bad gateway</html>", None);
        assert_eq!(failure.kind, UNAVAILABLE);
        assert!(failure.message.contains("bad gateway"));
        assert_eq!(failure.http_status, 502);
    }
}
