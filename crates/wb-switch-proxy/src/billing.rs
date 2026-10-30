//! billing 域的读/写动作：积分查询、签到、token 刷新。
//!
//! 与 Python 的 admin/pool.py + converter._refresh 同一批端点。全部直连 billing
//! 域（默认 www.codebuddy.cn），不走 chat 域，否则会被当成网关流量。
//!
//! 写操作（签到）会改远端状态；刷新 token 默认**不写回账号库**（灰度期账号库
//! 由桌面端与 Python 管），只把结果返回给调用方。

use std::time::Duration;

use serde_json::{json, Value};

use crate::config::ProxyConfig;

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("billing client")
    })
}

fn headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue};
    let mut map = HeaderMap::new();
    map.insert("content-type", HeaderValue::from_static("application/json"));
    map.insert("accept", HeaderValue::from_static("application/json"));
    if let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) {
        map.insert("authorization", value);
    }
    map
}

fn url(cfg: &ProxyConfig, path: &str) -> String {
    format!(
        "{}/v2/billing/meter/{}",
        cfg.billing_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

async fn post(cfg: &ProxyConfig, path: &str, token: &str, body: Value) -> Result<Value, String> {
    let response = client()
        .post(url(cfg, path))
        .headers(headers(token))
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("billing 请求失败：{error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
    if !status.is_success() {
        return Err(format!("billing HTTP {}：{}", status.as_u16(), parsed));
    }
    Ok(parsed)
}

/// 剩余积分：累加各积分包的余量。解析不出就报错，不伪造 0。
pub async fn credits(cfg: &ProxyConfig, token: &str) -> Result<Value, String> {
    let body = post(cfg, "get-user-resource", token, json!({})).await?;
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "billing code {code}：{}",
            body.get("msg").and_then(Value::as_str).unwrap_or("")
        ));
    }
    let accounts = body
        .get("data")
        .and_then(|data| data.get("Response"))
        .and_then(|response| response.get("Data"))
        .and_then(|data| data.get("Accounts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut remaining = 0.0_f64;
    for account in &accounts {
        let value = account
            .get("CycleCapacityRemainPrecise")
            .or_else(|| account.get("CapacityRemainPrecise"))
            .and_then(|value| {
                value
                    .as_f64()
                    .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
            })
            .unwrap_or(0.0);
        remaining += value;
    }
    Ok(json!({
        "remaining": (remaining * 100.0).round() / 100.0,
        "packages": accounts.len(),
    }))
}

/// 每日签到。`10001` = 今天已经签过。
pub async fn checkin(cfg: &ProxyConfig, token: &str) -> Result<Value, String> {
    let body = post(cfg, "daily-checkin", token, json!({})).await?;
    let code = body.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == 10001 {
        return Ok(json!({"status": "already"}));
    }
    if code != 0 {
        let message = body
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("签到失败");
        return Err(message.to_string());
    }
    let data = body.get("data").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "status": "checked-in",
        "credit": data.get("credit").and_then(Value::as_f64).unwrap_or(0.0),
        "streakDays": data.get("streak_days").and_then(Value::as_i64),
    }))
}

/// 刷新 access token（不写回账号库）。
pub async fn refresh_token(cfg: &ProxyConfig, refresh_token: &str) -> Result<Value, String> {
    let url = format!(
        "{}/v2/plugin/auth/token/refresh",
        cfg.billing_base.trim_end_matches('/')
    );
    let mut map = headers("");
    if let Ok(value) = reqwest::header::HeaderValue::from_str(refresh_token) {
        map.insert("x-refresh-token", value);
    }
    let response = client()
        .post(url)
        .headers(map)
        .json(&json!({}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("刷新请求失败：{error}"))?;
    let text = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
    if parsed.get("code").and_then(Value::as_i64) != Some(0) {
        return Err(format!("刷新失败：{}", parsed));
    }
    let data = parsed.get("data").cloned().unwrap_or_else(|| json!({}));
    Ok(json!({
        "refreshed": true,
        "expiresIn": data.get("expiresIn").cloned().unwrap_or(Value::Null),
        "persisted": false,
    }))
}
