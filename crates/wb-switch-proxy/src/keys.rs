//! 数据面密钥校验：哈希比对 + 启用/过期/模型白名单/额度 + 来源 IP 白名单。
//!
//! 与 Python 版 keysvc 的口径一致（错误码与文案也照搬）：
//!   401 密钥不存在 / 403 已停用、过期、IP 不在白名单 / 400 模型不在白名单 / 429 额度用尽。
//!
//! IP 白名单（[`ip_allowed`]）需要 axum 的 `ConnectInfo`，由 `http.rs` 采集来源地址后
//! 传进来；拿不到来源时**放行**——白名单是「额外限制」，不该因为读不到 peer 就把
//! 所有请求关死（那会让配置错误表现成「服务挂了」）。

use std::net::IpAddr;
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

/// 判断来源地址是否满足该密钥的 IP 白名单。
///
/// 规则（与 Python `keysvc.ip_allowed` 同口径）：
/// - 白名单为空 → 不限制；
/// - 列表里出现来源 IP 的字面值 → 通过；
/// - 列表里出现 CIDR（`10.0.0.0/8`）且来源落在网段内 → 通过；
/// - 来源未知（拿不到 `ConnectInfo`）→ 放行，理由见模块注释。
///
/// 回环地址单列出来：用户在自己机器上调试时写的是 `127.0.0.1`，而浏览器/CLI 可能
/// 从 `::1` 连进来。两者等价，不然「本机不许访问」会让排障变成猜谜。
pub fn ip_allowed(allowlist: &[String], peer: Option<IpAddr>) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    let Some(peer) = peer else {
        return true;
    };
    allowlist.iter().any(|entry| entry_matches(entry.trim(), peer))
}

fn entry_matches(entry: &str, peer: IpAddr) -> bool {
    if entry.is_empty() {
        return false;
    }
    if let Some((network, prefix)) = entry.split_once('/') {
        let (Ok(network), Ok(prefix)) = (network.trim().parse::<IpAddr>(), prefix.trim().parse::<u8>())
        else {
            return false;
        };
        return cidr_contains(network, prefix, peer);
    }
    match entry.parse::<IpAddr>() {
        Ok(ip) => ip == peer || (ip.is_loopback() && peer.is_loopback()),
        Err(_) => false,
    }
}

/// 判断 `peer` 是否落在 `network/prefix` 内。跨族（v4 网段 vs v6 来源）一律不匹配。
fn cidr_contains(network: IpAddr, prefix: u8, peer: IpAddr) -> bool {
    match (network, peer) {
        (IpAddr::V4(network), IpAddr::V4(peer)) => {
            if prefix > 32 {
                return false;
            }
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            (u32::from(network) & mask) == (u32::from(peer) & mask)
        }
        (IpAddr::V6(network), IpAddr::V6(peer)) => {
            if prefix > 128 {
                return false;
            }
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
            (u128::from(network) & mask) == (u128::from(peer) & mask)
        }
        _ => false,
    }
}

/// 校验请求密钥。返回 None 表示「这个实例还没有任何密钥」（全新安装放行）。
///
/// `peer` 是来源地址，用于该密钥自己的 `ip_allowlist`；见 [`ip_allowed`]。
pub fn authenticate(
    store: &StateStore,
    headers: &HeaderMap,
    model: Option<&str>,
    peer: Option<IpAddr>,
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

    // IP 白名单：放在模型/额度检查之前——来源不对时，连「这把钥匙能开哪些模型」都不该告诉它。
    let allowlist: Vec<String> = entry
        .get("ip_allowlist")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !ip_allowed(&allowlist, peer) {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "该密钥不允许从当前 IP 访问",
            "permission_error",
            "ip_not_allowed",
        ));
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
        assert!(authenticate(&store, &headers("any"), None, None).unwrap().is_none());
    }

    #[test]
    fn unknown_token_is_401() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({}))}));
        let error = authenticate(&store, &headers("wrong"), None, None).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn disabled_and_expired_are_403() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"enabled": false}))}));
        assert_eq!(authenticate(&store, &headers("secret"), None, None).unwrap_err().status(), StatusCode::FORBIDDEN);

        let (_dir2, store2) = store_with(json!({"k1": key_entry("secret", json!({"expires_at": 1}))}));
        assert_eq!(authenticate(&store2, &headers("secret"), None, None).unwrap_err().status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn model_whitelist_and_realm() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"models": ["glm-5.3"], "realm": "cn"}))}));
        let info = authenticate(&store, &headers("secret"), Some("glm-5.3"), None).unwrap().unwrap();
        assert_eq!(info.realm.as_deref(), Some("cn"));
        let error = authenticate(&store, &headers("secret"), Some("other"), None).unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn quota_exhausted_is_429() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"quota": 10, "used_tokens": 10}))}));
        assert_eq!(
            authenticate(&store, &headers("secret"), None, None).unwrap_err().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn empty_allowlist_means_no_restriction() {
        assert!(ip_allowed(&[], Some(ip("203.0.113.9"))));
        assert!(ip_allowed(&[], None));
    }

    #[test]
    fn exact_ip_match() {
        let list = vec!["203.0.113.9".to_string()];
        assert!(ip_allowed(&list, Some(ip("203.0.113.9"))));
        assert!(!ip_allowed(&list, Some(ip("203.0.113.10"))));
    }

    #[test]
    fn cidr_range_match() {
        let list = vec!["10.0.0.0/8".to_string()];
        assert!(ip_allowed(&list, Some(ip("10.1.2.3"))));
        assert!(!ip_allowed(&list, Some(ip("11.0.0.1"))));
        let narrow = vec!["192.168.1.0/24".to_string()];
        assert!(ip_allowed(&narrow, Some(ip("192.168.1.200"))));
        assert!(!ip_allowed(&narrow, Some(ip("192.168.2.1"))));
    }

    #[test]
    fn prefix_zero_matches_whole_family_but_not_other_family() {
        let list = vec!["0.0.0.0/0".to_string()];
        assert!(ip_allowed(&list, Some(ip("1.2.3.4"))));
        assert!(!ip_allowed(&list, Some(ip("::1"))), "v4 全放行不该顺带放行 v6");
    }

    #[test]
    fn ipv6_cidr_and_literal() {
        let list = vec!["2001:db8::/32".to_string()];
        assert!(ip_allowed(&list, Some(ip("2001:db8::5"))));
        assert!(!ip_allowed(&list, Some(ip("2001:db9::5"))));
        let literal = vec!["::1".to_string()];
        assert!(ip_allowed(&literal, Some(ip("::1"))));
    }

    #[test]
    fn loopback_is_equivalent_across_families() {
        // 用户写的 127.0.0.1，而客户端从 ::1 连进来，属同一台机器。
        let list = vec!["127.0.0.1".to_string()];
        assert!(ip_allowed(&list, Some(ip("::1"))));
    }

    #[test]
    fn unknown_peer_is_allowed() {
        // 拿不到来源时放行：白名单是额外限制，读不到 peer 不该把所有请求关死。
        let list = vec!["10.0.0.0/8".to_string()];
        assert!(ip_allowed(&list, None));
    }

    #[test]
    fn garbage_entries_do_not_match() {
        let list = vec![
            "".to_string(),
            "not-an-ip".to_string(),
            "10.0.0.0/99".to_string(),
            "10.0.0.0/".to_string(),
        ];
        assert!(!ip_allowed(&list, Some(ip("10.0.0.1"))));
    }

    #[test]
    fn auth_rejects_ip_outside_allowlist() {
        let (_dir, store) = store_with(json!({"k1": key_entry("secret", json!({"ip_allowlist": ["10.0.0.0/8"]}))}));
        let error = authenticate(&store, &headers("secret"), None, Some(ip("203.0.113.9"))).unwrap_err();
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        let ok = authenticate(&store, &headers("secret"), None, Some(ip("10.1.1.1"))).unwrap();
        assert!(ok.is_some());
    }
}
