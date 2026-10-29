//! 数据面 `GET /v1/usage`：网关密钥自查。
//!
//! 与 Python 版 `gateway_key_usage` 同契约（`reverse-proxy/admin/server.py`）：
//! 用**客户端自己的 API Key** 认证，返回该密钥的额度、有效期、模型范围与 IP 绑定。
//! 只读——不查上游、不产生计费调用，token 与积分一分不记。
//!
//! 注意这不是「服务用量统计」（那是管理面的 `/admin/api/usage/stats`）。
//!
//! 未实现（IP 绑定相关，随管理面白名单一起后置）：
//! - `ips.bound` 恒为空数组：`api_key_ips` 的写入依赖来源 IP，本阶段还没执行白名单；
//! - 入站日志与 0-token 请求日志：Rust 侧尚未接入 `log_inbound` 等价实现。

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::state::StateStore;

/// 与 `keys::authenticate` 同口径的 401 形态，便于客户端统一处理。
fn deny(message: &str, code: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {"message": message, "type": "auth_error", "code": code}
        })),
    )
        .into_response()
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 从 `Authorization: Bearer xxx` 或 `x-api-key` 取密钥原文。
fn token_of(headers: &HeaderMap) -> String {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.to_ascii_lowercase().starts_with("bearer "))
        .map(|value| value[7..].trim().to_string());
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string());
    bearer
        .filter(|value| !value.is_empty())
        .or(api_key)
        .unwrap_or_default()
}

fn int_of(entry: &Value, key: &str) -> i64 {
    entry.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn float_of(entry: &Value, key: &str) -> f64 {
    entry.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// 把 `state.json` 里的密钥条目转成 `/v1/usage` 的响应体。
///
/// 单独拿出来是为了能脱离 HTTP 测试：这是一段纯映射，字段漏一个就是契约不符。
pub fn usage_of(store: &StateStore, key_id: &str, entry: &Value) -> Value {
    let quota = int_of(entry, "quota");
    let used_tokens = int_of(entry, "used_tokens");
    let quota_credit = float_of(entry, "quota_credit");
    let used_credit = round6(float_of(entry, "used_credit"));

    let bound = store
        .get("api_key_ips")
        .and_then(|value| value.get(key_id).cloned())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();

    json!({
        "object": "usage",
        "key": {
            "id": entry.get("id").cloned().unwrap_or_else(|| json!(key_id)),
            "name": entry.get("name").and_then(Value::as_str).unwrap_or(""),
            "prefix": entry.get("prefix").and_then(Value::as_str).unwrap_or(""),
            "hint": entry.get("hint").and_then(Value::as_str).unwrap_or(""),
            "enabled": entry.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            "expires_at": entry.get("expires_at").cloned().unwrap_or(Value::Null),
            "realm": entry.get("realm").and_then(Value::as_str).unwrap_or(""),
            "models": entry.get("models").and_then(Value::as_array).cloned().unwrap_or_default(),
            "created": entry.get("created").cloned().unwrap_or(Value::Null),
            "last_used_at": entry.get("last_used_at").cloned().unwrap_or(Value::Null),
        },
        "tokens": {
            "limit": quota,
            "used": used_tokens,
            // 0 表示不限；不限时不给 remaining，免得下游把 null 当成 0 处理。
            "remaining": if quota > 0 { json!((quota - used_tokens).max(0)) } else { Value::Null },
        },
        "credit": {
            "limit": quota_credit,
            "used": used_credit,
            "remaining": if quota_credit > 0.0 {
                json!(round6((quota_credit - used_credit).max(0.0)))
            } else {
                Value::Null
            },
        },
        "ips": {
            "max": int_of(entry, "max_ips"),
            "allowlist": entry.get("ip_allowlist").and_then(Value::as_array).cloned().unwrap_or_default(),
            "bound": bound,
        },
    })
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

/// 处理 `GET /v1/usage`。
pub fn handle(store: &StateStore, headers: &HeaderMap) -> Response {
    let token = token_of(headers);
    if token.is_empty() {
        return deny("缺少 API Key", "missing");
    }
    let keys = store.get("keys").unwrap_or_else(|| json!({}));
    let keys = keys.as_object().cloned().unwrap_or_default();
    // 与 keys::authenticate 同一个特例：全新安装还没有任何密钥时不做限制。
    if keys.is_empty() {
        return Json(json!({
            "object": "usage",
            "key": null,
            "unrestricted": true,
            "note": "临时测试密钥没有策略限制",
        }))
        .into_response();
    }
    let digest = sha256_hex(&token);
    let Some((id, entry)) = keys
        .iter()
        .find(|(_, entry)| entry.get("hash").and_then(Value::as_str) == Some(digest.as_str()))
    else {
        return deny("invalid api key", "invalid");
    };
    if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
        return deny("该密钥已停用", "key_disabled");
    }
    Json(usage_of(store, id, entry)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateStore;

    fn store_with(entries: Value) -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, json!({"keys": entries}).to_string()).unwrap();
        let store = StateStore::open_writable(path).unwrap();
        (dir, store)
    }

    fn secret_entry(token: &str) -> Value {
        json!({
            "hash": sha256_hex(token),
            "name": "测试密钥",
            "prefix": "wbk_abc",
            "enabled": true,
            "quota": 1000,
            "used_tokens": 300,
            "quota_credit": 10.5,
            "used_credit": 2.25,
            "max_ips": 2,
            "ip_allowlist": ["127.0.0.1"],
            "models": ["glm-5.3"],
        })
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn missing_token_is_401() {
        let (_dir, store) = store_with(json!({}));
        let response = handle(&store, &HeaderMap::new());
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn unknown_token_is_401() {
        let (_dir, store) = store_with(json!({"k1": secret_entry("secret")}));
        let response = handle(&store, &headers("wrong"));
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn x_api_key_header_also_works() {
        let (_dir, store) = store_with(json!({"k1": secret_entry("secret")}));
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "secret".parse().unwrap());
        assert_eq!(handle(&store, &headers).status(), StatusCode::OK);
    }

    #[test]
    fn disabled_key_is_401() {
        let mut entry = secret_entry("secret");
        entry["enabled"] = json!(false);
        let (_dir, store) = store_with(json!({"k1": entry}));
        assert_eq!(handle(&store, &headers("secret")).status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn no_keys_means_unrestricted() {
        let (_dir, store) = store_with(json!({}));
        let response = handle(&store, &headers("anything"));
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn usage_maps_quota_and_remaining() {
        let (_dir, store) = store_with(json!({"k1": secret_entry("secret")}));
        let usage = usage_of(&store, "k1", &secret_entry("secret"));
        assert_eq!(usage["tokens"]["limit"], 1000);
        assert_eq!(usage["tokens"]["used"], 300);
        assert_eq!(usage["tokens"]["remaining"], 700);
        assert_eq!(usage["credit"]["remaining"], 8.25);
        assert_eq!(usage["ips"]["max"], 2);
        assert_eq!(usage["key"]["models"][0], "glm-5.3");
    }

    #[test]
    fn unlimited_quota_has_null_remaining() {
        let mut entry = secret_entry("secret");
        entry["quota"] = json!(0);
        entry["quota_credit"] = json!(0);
        let (_dir, store) = store_with(json!({"k1": entry.clone()}));
        let usage = usage_of(&store, "k1", &entry);
        assert!(usage["tokens"]["remaining"].is_null());
        assert!(usage["credit"]["remaining"].is_null());
    }
}
