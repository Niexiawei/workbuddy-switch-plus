//! 对话活跃上报：`POST {BASE}/v2/report`。
//!
//! 协议对照 Python 的 `reverse-proxy/admin/report.py` 与 `tests/test_report.py`
//! （那份测试是唯一权威口径，54 KB 全是在钉这一件事）。要点如下，每一条都对应
//! 一个「错了就静默失效」的坑：
//!
//! - **body 是数组**，一条上报 = 数组里一个 `chat_request_send` 事件。上游是
//!   每条一个请求（不是一次塞 N 条），N 条之间隔 1.5 秒。
//! - **事件必须是完整字段形状**（36 个键），不能用「只发必要字段」的精简体：
//!   官方客户端就发这一整套，精简版会被上游后续加严挡掉。
//! - **`userId` 必填**（= 账号 uid）。缺了它服务端照样回 200，但把事件**静默丢弃**
//!   ——这是这个接口最阴的地方：HTTP 200 不等于上报生效。所以发完之后要回读
//!   `/activity/growth/streak` 自检（[`streak`]）。
//! - **N 条共用同一个 `conversationId`**，`requestId` 各自独立——模拟同一会话内
//!   N 轮对话。领养猫的对话量门槛（chat_5 前置要 5 次对话）就是靠这个补满的。
//! - 风控口径：**每号每天一轮即可**，不做多时点高频上报。
//!
//! 注意它和旅行是**两个域**：travel 走 growth（带 web 三头），report 走 billing
//! （就是签到用的那套头），所以这里复用账号池给的鉴权头，**不额外加**
//! `Origin` / `Referer` / `x-client-platform`。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Map, Value};

/// 上报接口。挂在 billing 域（与签到同一个域）。
pub const REPORT_PATH: &str = "/v2/report";
/// 连登天数（只读 oracle）：上报之后回读它，才能发现「200 但静默丢弃」。
pub const STREAK_PATH: &str = "/activity/growth/streak";

/// 默认主机：与 `billing.rs` 的 `DEFAULT_BILLING` 同源。
const DEFAULT_BASE: &str = "https://www.codebuddy.cn";

/// 每天每号发几条。默认 1（点亮连登 + 解锁领养前置）；上限 5（领养猫要 5 次对话）。
pub const DEFAULT_REPORTS: i64 = 1;
pub const MAX_REPORTS: i64 = 5;
pub const MIN_REPORTS: i64 = 1;

/// 同一账号内两条之间的间隔。5 连发是在模拟同一会话的多轮对话，连发会触发上游风控。
pub const REPORT_GAP_SECONDS: f64 = 1.5;

/// 事件里的固定字段：照抄官方客户端 `chat_request_send` 的形状。
/// 模型名等不要改——它模拟的是官方客户端事件，换成别的值上游可能不认。
const EVENT_CODE: &str = "chat_request_send";
const MODE: &str = "craft";
const INPUT_LENGTH: i64 = 12;
const REQUEST_MODEL_ID: &str = "deepseek-v4-flash";
const REQUEST_MODEL_NAME: &str = "DeepSeek V4 Flash";
const AGENT_NAME: &str = "default";
const AGENT_TYPE: &str = "conversation";

/// `conversationId` 前缀。上游用 `wb2api-<ms>`；这边换个前缀便于日志里区分来源。
const CONVERSATION_PREFIX: &str = "wb-switch";

fn base() -> String {
    std::env::var("WB_PROXY_BILLING")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE.to_string())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

/// 本轮上报共用的会话标识。
pub fn conversation_id(now_ms: i64) -> String {
    format!("{CONVERSATION_PREFIX}-{now_ms}")
}

/// 一条 `chat_request_send` 事件（36 个字段，与官方客户端对齐，**勿精简**）。
pub fn chat_event(uid: &str, cid: &str, rid: &str, now_ms: i64) -> Value {
    let mut event = Map::new();
    event.insert("eventCode".into(), json!(EVENT_CODE));
    event.insert("timestamp".into(), json!(now_ms));
    event.insert("reportDelay".into(), json!(0));
    event.insert("mode".into(), json!(MODE));
    event.insert("conversationId".into(), json!(cid));
    event.insert("requestId".into(), json!(rid));
    event.insert("inputLength".into(), json!(INPUT_LENGTH));
    event.insert("requestModelId".into(), json!(REQUEST_MODEL_ID));
    event.insert("requestModelName".into(), json!(REQUEST_MODEL_NAME));
    event.insert("isPlan".into(), json!(false));
    event.insert("isAutoExecuteTerminal".into(), json!(false));
    event.insert("isAutoModify".into(), json!(false));
    event.insert("codebaseEnable".into(), json!(false));
    event.insert("maxToken".into(), json!(0));
    event.insert("maxSteps".into(), json!(0));
    event.insert("temperature".into(), json!(0));
    event.insert("maxRetries".into(), json!(0));
    event.insert("mentionContexts".into(), json!([]));
    event.insert("knowledgeId".into(), json!([]));
    event.insert("knowledgeName".into(), json!([]));
    event.insert("codebaseId".into(), json!(""));
    event.insert("mentionContextCount".into(), json!(0));
    event.insert("command".into(), json!(""));
    event.insert("expertId".into(), json!(""));
    event.insert("recommendId".into(), json!(""));
    event.insert("skillId".into(), json!(""));
    event.insert("skillCount".into(), json!(0));
    event.insert("totalCount".into(), json!(0));
    event.insert("fileUri".into(), json!(""));
    event.insert("presentAt".into(), json!(now_ms));
    event.insert("traceId".into(), json!(""));
    event.insert("rootRequestId".into(), json!(cid));
    event.insert("parentConversationId".into(), json!(cid));
    event.insert("agentName".into(), json!(AGENT_NAME));
    event.insert("agentType".into(), json!(AGENT_TYPE));
    // 必填。缺了它服务端 200 但静默丢弃，所以由调用方从鉴权头里取，
    // 保证 body 里的 userId 和请求头里的 X-User-Id 永远是同一个值。
    event.insert("userId".into(), json!(uid));
    Value::Object(event)
}

/// 本轮要发的 N 条事件：同 cid，requestId 各自独立。
pub fn report_events(uid: &str, cid: &str, count: i64, now_ms: i64) -> Vec<Value> {
    (1..=clamp_count(count))
        .map(|index| chat_event(uid, cid, &format!("{cid}-r{index}"), now_ms))
        .collect()
}

/// 条数落在 `[1, 5]`。超出不报错、直接夹住——上游口径就是这个区间。
///
/// 垃圾输入退回默认值而不是抛异常：界面上传了个空字符串不该把调度打挂。
pub fn clamp_count(count: impl Into<i64>) -> i64 {
    count.into().clamp(MIN_REPORTS, MAX_REPORTS)
}

/// 从鉴权头取 uid（大小写都认）。
pub fn uid_of(headers: &HashMap<String, String>) -> String {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("x-user-id"))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("report client")
    })
}

fn url(path: &str) -> String {
    format!("{}{path}", base().trim_end_matches('/'))
}

/// 发一次请求，信封口径与 Python `upstream.envelope()` 对齐：
/// `{"code": 0, ...}` 表示成功；HTTP 错误时返回 `{"code": <status>, "message": <body 前若干字符>}`。
async fn send(
    method: reqwest::Method,
    path: &str,
    headers: &HashMap<String, String>,
    body: Option<Value>,
) -> Value {
    let mut request = client().request(method, url(path)).timeout(Duration::from_secs(20));
    for (key, value) in headers {
        request = request.header(key.as_str(), value.as_str());
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&text).ok();
            match parsed {
                Some(value) if status < 300 => value,
                Some(value) => value,
                None => json!({
                    "code": status,
                    "message": if status >= 300 {
                        text.chars().take(200).collect::<String>()
                    } else {
                        String::new()
                    },
                }),
            }
        }
        Err(error) => json!({"code": -1, "message": format!("{error}")}),
    }
}

fn is_ok(envelope: &Value) -> bool {
    matches!(
        envelope.get("code").and_then(Value::as_i64),
        Some(0) | Some(200)
    )
}

fn message_of(envelope: &Value, fallback: &str) -> String {
    envelope
        .get("message")
        .or_else(|| envelope.get("msg"))
        .and_then(Value::as_str)
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// 跑一轮上报，返回 `{ok, sent, count, conversationId, message, skip?}`。
///
/// 逐条发、逐条判：某一条失败就停在本号，不把剩下的续发出去——续发既没必要
/// （服务端按天幂等），还会在已经出问题的号上多打几次。
///
/// 条与条之间的节流**必须在请求之前**：`sleep` 放在发送之后等于没节流，
/// 而它防的是「同一账号秒发 N 条」这条风控信号，位置错了就等于没写。
pub async fn report(headers: &HashMap<String, String>, count: i64) -> Value {
    let uid = uid_of(headers);
    if uid.is_empty() {
        // 没有 uid 一定会被静默丢弃，连请求都不用发。
        return json!({
            "ok": false, "sent": 0, "count": 0, "conversationId": "",
            "skip": "no-uid",
            "message": "凭据里没有账号标识（uid），已跳过",
        });
    }

    let cid = conversation_id(now_ms());
    let events = report_events(&uid, &cid, count, now_ms());
    let total = events.len() as i64;
    let mut sent = 0_i64;
    for (index, event) in events.into_iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(Duration::from_secs_f64(REPORT_GAP_SECONDS)).await;
        }
        let envelope = send(reqwest::Method::POST, REPORT_PATH, headers, Some(json!([event]))).await;
        if !is_ok(&envelope) {
            return json!({
                "ok": false, "sent": sent, "count": total, "conversationId": cid,
                "message": message_of(&envelope, "活跃上报失败"),
            });
        }
        sent += 1;
    }
    json!({
        "ok": true, "sent": sent, "count": total, "conversationId": cid,
        "message": format!("活跃上报完成（{sent} 条）"),
    })
}

/// 回读连登天数：`{ok, days, message}`。
///
/// 这就是「上报 200 但静默丢弃」的探针——上报成功而 days 仍是 0，说明事件被
/// 丢了（最常见的原因就是 userId 缺失或不对）。
///
/// 解析必须容错：这个函数是探针，它读不出来只说明「探不了」，绝不能反过来把
/// 已经成功的上报判成失败。读不出来时返回 `ok=false`（而不是 `days=0`），
/// 调用方据此区分「探针坏了」与「读到了 0 = 事件真被丢了」。
pub async fn streak(headers: &HashMap<String, String>) -> Value {
    let envelope = send(reqwest::Method::GET, STREAK_PATH, headers, None).await;
    if !is_ok(&envelope) {
        return json!({
            "ok": false, "days": 0,
            "message": message_of(&envelope, "连登状态查询失败"),
        });
    }
    let days = envelope
        .get("data")
        .and_then(|data| data.get("streak"))
        .and_then(|streak| streak.get("days"))
        .and_then(|days| {
            days.as_i64().or_else(|| {
                days.as_str()
                    .and_then(|text| text.trim().parse().ok())
            })
        });
    match days {
        Some(days) => json!({"ok": true, "days": days, "message": ""}),
        // 形状对不上：给「探不了」，而不是假装读到 0。
        None => json!({
            "ok": false, "days": 0,
            "message": "连登天数格式异常，本次不做判断",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_carries_the_full_field_shape() {
        let event = chat_event("u1", "cid", "cid-r1", 1_800_000_000_000);
        let object = event.as_object().expect("event must be an object");
        // Python 侧断言恰好 36 个键；少一个都是「精简版会被上游挡掉」。
        assert_eq!(object.len(), 36, "事件字段数必须与官方客户端一致");
        for key in [
            "eventCode",
            "userId",
            "conversationId",
            "requestId",
            "rootRequestId",
            "parentConversationId",
            "agentName",
            "agentType",
            "presentAt",
        ] {
            assert!(object.contains_key(key), "缺字段 {key}");
        }
    }

    #[test]
    fn event_uses_the_values_the_official_client_sends() {
        let event = chat_event("u1", "cid", "cid-r1", 1);
        assert_eq!(event["eventCode"], json!("chat_request_send"));
        assert_eq!(event["mode"], json!("craft"));
        assert_eq!(event["inputLength"], json!(12));
        assert_eq!(event["requestModelId"], json!("deepseek-v4-flash"));
        assert_eq!(event["requestModelName"], json!("DeepSeek V4 Flash"));
        assert_eq!(event["agentName"], json!("default"));
        assert_eq!(event["agentType"], json!("conversation"));
        assert_eq!(event["userId"], json!("u1"));
    }

    #[test]
    fn events_share_one_conversation_but_not_request_ids() {
        let events = report_events("u1", "cid", 5, 1_800_000_000_000);
        assert_eq!(events.len(), 5);
        let cids: std::collections::HashSet<_> =
            events.iter().map(|e| e["conversationId"].clone()).collect();
        assert_eq!(cids.len(), 1, "N 条必须共用一个会话");
        let rids: std::collections::HashSet<_> =
            events.iter().map(|e| e["requestId"].clone()).collect();
        assert_eq!(rids.len(), 5, "requestId 必须各自独立");
        assert_eq!(events[0]["requestId"], json!("cid-r1"));
        assert_eq!(events[4]["requestId"], json!("cid-r5"));
    }

    #[test]
    fn conversation_id_is_prefixed_and_time_stamped() {
        assert_eq!(conversation_id(1_800_000_000_000), "wb-switch-1800000000000");
    }

    #[test]
    fn clamp_count_holds_the_one_to_five_window() {
        assert_eq!(clamp_count(1), 1);
        assert_eq!(clamp_count(5), 5);
        assert_eq!(clamp_count(0), 1);
        assert_eq!(clamp_count(-3), 1);
        assert_eq!(clamp_count(99), 5);
    }

    #[test]
    fn uid_of_reads_x_user_id_case_insensitively() {
        let mut headers = HashMap::new();
        headers.insert("x-user-id".to_string(), "u1".to_string());
        assert_eq!(uid_of(&headers), "u1");

        let mut headers = HashMap::new();
        headers.insert("X-User-Id".to_string(), "u2".to_string());
        assert_eq!(uid_of(&headers), "u2");

        assert_eq!(uid_of(&HashMap::new()), "");
    }
}
