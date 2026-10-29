//! 数据面密钥校验：哈希比对 + 启用/过期/模型白名单/额度。
//!
//! 与 Python 版 keysvc 的口径一致（错误码与文案也照搬）：
//!   401 密钥不存在 / 403 已停用或过期 / 400 模型不在白名单 / 429 额度用尽。
//! 未实现：IP 白名单与来源 IP 计数（需要 axum ConnectInfo，留到 P5 管理面一起做）。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::state::StateStore;

#[derive(Clone, Debug)]
pub struct KeyInfo {
    pub id: String,
    pub name: String,
    /// `cn` / `ai`；None 表示不限制。
    pub realm: Option<String>,
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|value| value.to_str().ok()))
        .unwrap_or("")
        .trim()
        .to_string()
}

fn reject(status: StatusCode, message: &str, kind: &str, code: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": kind, "code": code}})),
    )
        .into_response()
}

fn now_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_secs()).unwrap_or(0)
}

/// 校验请求密钥。返回 None 表示「这个实例还没有任何密钥」（全新安装放行）。
pub fn authenticate(
    store: &StateStore,
    headers: &HeaderMap,
    model: Option<&str>,
) -> Result<Option<KeyInfo>, Response> {
    let keys_value = store.get("keys").unwrap_or_else(|| json!({}));
    let keys = keys_value.as_object().cloned().unwrap_or_default();
    if keys.is_empty() {
        return Ok(None);
    }
    let token = token_of(headers);
    if token.is_empty() {
        return Err(reject(StatusCode::UNAUTHORIZED, "缺少 API Key", "authentication_error", "missing_key"));
    }
    let digest = sha256_hex(&token);
    let Some((id, entry)) = keys.iter().find(|(_, entry)| {
        entry.get("hash").and_then(Value::as_str) == Some(digest.as_str())
    }) else {
        return Err(reject(StatusCode::UNAUTHORIZED, "invalid api key", "authentication_error", "invalid_key"));
    };

    if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
        return Err(reject(StatusCode::FORBIDDEN, "该密钥已停用", "permission_error", "key_disabled"));
    }
    if let Some(expires) = entry.get("expires_at").and_then(Value::as_i64) {
        if expires > 0 && expires as u64 <= now_seconds() {
            return Err(reject(StatusCode::FORBIDDEN, "该密钥已过期", "permission_error", "key_expired"));
        }
    }

    if let Some(model) = model {
        let allowed = entry.get("models").and_then(Value::as_array);
        if let Some(allowed) = allowed {
            if !allowed.is_empty() {
                let hit = allowed.iter().filter_map(Value::as_str).any(|item| item == model);
                if !hit {
                    return Err(reject(
                        StatusCode::BAD_REQUEST,
                        &format!("该密钥不允许使用模型 {model}"),
                        "invalid_request_error",
                        "model_not_allowed",
                    ));
                }
            }
        }
    }

    let quota = entry.get("quota").and_then(Value::as_i64).unwrap_or(0);
    let used = entry.get("used_tokens").and_then(Value::as_i64).unwrap_or(0);
    if quota > 0 && used >= quota {
        return Err(reject(
            StatusCode::TOO_MANY_REQUESTS,
            "该密钥的 token 额度已用尽",
            "insufficient_quota",
            "insufficient_quota",
        ));
    }

    let realm = entry
        .get("realm")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(Some(KeyInfo {
        id: id.clone(),
        name: entry.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
        realm,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn store_with(keys: Value) -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, serde_json::to_string(&json!({"keys": keys})).unwrap()).unwrap();
        (dir, StateStore::open(&path).unwrap())
    }

    fn headers(token: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert("authorization", HeaderValue::from_str(&format!("Bearer {token}")).unwrap());
        map
    }

    fn key_entry(token: &str, extra: Value) -> Value {
        let mut entry = json!({"hash": sha256_hex(token), "name": "k", "enabled": true});
        if let (Some(target), Some(source)) = (entry.as_object_mut(), extra.as_object()) {
            for (name, value) in source {
                target.insert(name.clone(), value.clone());
            }
        }
        entry
    }

    #[test]
    fn empty_instance_allows_everything() {
        let (_dir, store) = store_with(json!({}));
        assert!(authenticate(&store, &headers("any"), None).unwrap().is_none());
    }

    #[test]
    fn unknown_token_is_401() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({}))}));
        let error = authenticate(&store, &headers("wrong"), None).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn disabled_and_expired_are_403() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"enabled": false}))}));
        assert_eq!(authenticate(&store, &headers("secret"), None).unwrap_err().status(), StatusCode::FORBIDDEN);

        let (_dir2, store2) = store_with(json!({"k1": key_entry("secret", json!({"expires_at": 1}))}));
        assert_eq!(authenticate(&store2, &headers("secret"), None).unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn model_whitelist_and_realm() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"models": ["glm-5.3"], "realm": "cn"}))}));
        let info = authenticate(&store, &headers("secret"), Some("glm-5.3")).unwrap().unwrap();
        assert_eq!(info.realm.as_deref(), Some("cn"));
        let error = authenticate(&store, &headers("secret"), Some("other")).unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn quota_exhausted_is_429() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"quota": 10, "used_tokens": 10}))}));
        assert_eq!(
            authenticate(&store, &headers("secret"), None).unwrap_err().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
