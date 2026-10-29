//! 账号池：选号、冷却、亲和与失败记录。
//!
//! 灰度期 Rust 只读 state.json（Python 是唯一写者），所以运行态全部放在内存：
//! 重启后从 account_status 重新播种，不会与 Python 的写入打架。
//!
//! 与 Python 的差异（P4 明确保留）：
//!   · 并发闸门只做进程内计数，跨进程不共享；
//!   · 额度耗尽判断依赖 account_status 里的 remaining / credits_updated 快照；
//!   · 会话亲和只在内存里保留 24h，不落盘。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::accounts::{self, AccountView};
use crate::errors::{self, Failure};
use crate::state::StateStore;

#[derive(Clone, Debug, Default)]
struct RuntimeStatus {
    cooldown_until: u64,
    model_cooldown: HashMap<String, u64>,
    consecutive_fails: i64,
    request_count: i64,
    inflight: u32,
    remaining: Option<f64>,
    credits_updated: u64,
    last_error: Option<String>,
}

#[derive(Default)]
struct Inner {
    cursor: usize,
    status: HashMap<String, RuntimeStatus>,
    bindings: HashMap<String, (String, u64)>,
}

#[derive(Debug)]
pub enum PoolError {
    NoAccount(String),
}

pub struct Pool {
    store: Arc<StateStore>,
    accounts_path: PathBuf,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    inner: Mutex<Inner>,
}

pub struct Selected {
    pub account: AccountView,
    pub token: String,
    pub account_id: String,
}

impl Pool {
    pub fn new(store: Arc<StateStore>, accounts_path: PathBuf) -> Self {
        let mut inner = Inner::default();
        let status_value = store.get("account_status").unwrap_or_else(|| serde_json::json!({}));
        if let Some(status) = status_value.as_object() {
            for (aid, entry) in status {
                let mut runtime = RuntimeStatus::default();
                runtime.cooldown_until = entry.get("cooldown_until").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                runtime.consecutive_fails = entry.get("consecutive_fails").and_then(Value::as_i64).unwrap_or(0);
                runtime.request_count = entry.get("request_count").and_then(Value::as_i64).unwrap_or(0);
                runtime.remaining = entry.get("remaining").and_then(Value::as_f64);
                runtime.credits_updated = entry.get("credits_updated").and_then(Value::as_f64).unwrap_or(0.0) as u64;
                runtime.last_error = entry.get("last_error").and_then(Value::as_str).map(str::to_string);
                inner.status.insert(aid.clone(), runtime);
            }
        }
        Self {
            store,
            accounts_path,
            clock: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_secs())
                    .unwrap_or(0)
            }),
            inner: Mutex::new(inner),
        }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn now(&self) -> u64 {
        (self.clock)()
    }

    /// 选一个可用账号。\`affinity\` 通常来自会话 id，缺省时不粘会话。
    pub fn select(&self, model: &str, affinity: Option<&str>, realm: Option<&str>) -> Result<Selected, PoolError> {
        let now = self.now();
        let snapshot = self.store.snapshot();
        let overrides = snapshot.get("account_overrides").and_then(Value::as_object);
        let raw = wb_switch_core::modules::account::load_accounts_at(&self.accounts_path);
        let views = accounts::list_at(&self.accounts_path);

        let mut inner = self.inner.lock().unwrap();
        inner.bindings.retain(|_, (_, expires)| *expires > now);

        let mut candidates: Vec<(AccountView, String)> = Vec::new();
        for view in views {
            if let Some(override_row) = overrides.and_then(|map| map.get(&view.uid)) {
                if override_row.get("hidden").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if override_row.get("enabled").and_then(Value::as_bool) == Some(false) {
                    continue;
                }
            }
            if let Some(wanted) = realm {
                if view.variant != wanted {
                    continue;
                }
            }
            let Some(record) = raw.iter().find(|row| {
                row.get("uid").and_then(Value::as_str) == Some(view.uid.as_str())
                    || row.get("id").and_then(Value::as_str) == Some(view.id.as_str())
            }) else { continue };
            let Some(token) = record.get("access_token").and_then(Value::as_str) else { continue };
            if token.trim().is_empty() {
                continue;
            }
            let status = inner.status.get(&view.uid).cloned().unwrap_or_default();
            if status.cooldown_until > now {
                continue;
            }
            if status.model_cooldown.get(model).copied().unwrap_or(0) > now {
                continue;
            }
            // 额度耗尽：只认 10 分钟内的快照，过期快照不能当判据。
            if status.remaining == Some(0.0) && status.credits_updated + 600 > now {
                continue;
            }
            candidates.push((view, token.to_string()));
        }

        if candidates.is_empty() {
            return Err(PoolError::NoAccount(
                "暂无可用账号：请检查暂停、积分、冷却或登录状态".to_string(),
            ));
        }

        // 会话亲和：绑定还在且账号仍可用就继续用它。
        if let Some(key) = affinity {
            if let Some((aid, _)) = inner.bindings.get(key) {
                if let Some(found) = candidates.iter().find(|(view, _)| &view.uid == aid) {
                    return Ok(Selected { account: found.0.clone(), token: found.1.clone(), account_id: found.0.uid.clone() });
                }
            }
        }

        let routing = snapshot
            .get("pool")
            .and_then(|pool| pool.get("routing"))
            .and_then(Value::as_str)
            .unwrap_or("round_robin");
        let chosen = match routing {
            "manual" => {
                let active = snapshot.get("active").and_then(Value::as_str).unwrap_or("");
                candidates
                    .iter()
                    .position(|(view, _)| view.uid == active || view.id == active)
                    .map(|index| index)
            }
            "least_used" => {
                let mut best: Option<(usize, i64)> = None;
                for (index, (view, _)) in candidates.iter().enumerate() {
                    let count = inner.status.get(&view.uid).map(|status| status.request_count).unwrap_or(0);
                    if best.map(|(_, best)| count < best).unwrap_or(true) {
                        best = Some((index, count));
                    }
                }
                best.map(|(index, _)| index)
            }
            _ => {
                let index = inner.cursor % candidates.len();
                inner.cursor = inner.cursor.wrapping_add(1);
                Some(index)
            }
        };

        let Some(index) = chosen else {
            return Err(PoolError::NoAccount("当前账号不可用：请检查账号池设置".to_string()));
        };
        let (view, token) = candidates.swap_remove(index);
        if let Some(key) = affinity {
            inner.bindings.insert(key.to_string(), (view.uid.clone(), now + 86_400));
        }
        Ok(Selected { account: view.clone(), token, account_id: view.uid })
    }

    pub fn acquire(&self, account_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.status.entry(account_id.to_string()).or_default().inflight += 1;
    }

    pub fn release(&self, account_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(status) = inner.status.get_mut(account_id) {
            status.inflight = status.inflight.saturating_sub(1);
        }
    }

    /// 记录一次请求结果：成功清失败计数，失败按分类写冷却（账号级 / 模型级）。
    pub fn record(&self, account_id: &str, status_code: u16, model: &str, failure: Option<&Failure>) {
        let now = self.now();
        let mut inner = self.inner.lock().unwrap();
        let status = inner.status.entry(account_id.to_string()).or_default();
        status.request_count += 1;
        if let Some(failure) = failure {
            if failure.account_cooldown > 0 {
                status.cooldown_until = status.cooldown_until.max(now + failure.account_cooldown);
            }
            if failure.model_cooldown > 0 {
                status.model_cooldown.insert(model.to_string(), now + failure.model_cooldown);
            }
            if matches!(failure.kind, errors::AUTH | errors::QUOTA | errors::RATE_LIMIT | errors::UNAVAILABLE) {
                status.consecutive_fails += 1;
            }
            status.last_error = Some(failure.message.chars().take(200).collect());
            return;
        }
        match status_code {
            200..=299 => {
                status.consecutive_fails = 0;
                status.last_error = None;
            }
            401 | 403 => status.cooldown_until = status.cooldown_until.max(now + 300),
            402 | 429 => status.cooldown_until = status.cooldown_until.max(now + 1800),
            500..=599 => status.cooldown_until = status.cooldown_until.max(now + 60),
            _ => {}
        }
    }

    /// 账号池视图：给管理面用的运行态字段（对齐 Python `pool.rows` 的关键项）。
    pub fn rows(&self, views: &[AccountView]) -> Vec<Value> {
        let now = self.now();
        let snapshot = self.store.snapshot();
        let overrides = snapshot.get("account_overrides").and_then(Value::as_object);
        let inner = self.inner.lock().unwrap();
        views
            .iter()
            .map(|view| {
                let status = inner.status.get(&view.uid).cloned().unwrap_or_default();
                let paused = overrides
                    .and_then(|map| map.get(&view.uid))
                    .and_then(|row| row.get("enabled"))
                    .and_then(Value::as_bool)
                    == Some(false);
                let state = if paused {
                    "paused"
                } else if status.cooldown_until > now {
                    "cooling"
                } else if status.remaining == Some(0.0) && status.credits_updated + 600 > now {
                    "exhausted"
                } else {
                    "available"
                };
                let model_cooldown: Vec<Value> = status
                    .model_cooldown
                    .iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(model, until)| json!({"model": model, "until": until}))
                    .collect();
                json!({
                    "id": view.id,
                    "uid": view.uid,
                    "name": view.nickname,
                    "variant": view.variant,
                    "domain": view.domain,
                    "enabled": !paused,
                    "pool_state": state,
                    "cooldown_until": status.cooldown_until,
                    "remaining": status.remaining,
                    "credits_updated": status.credits_updated,
                    "credits_stale": status.credits_updated + 600 <= now,
                    "last_error": status.last_error,
                    "request_count": status.request_count,
                    "consecutive_fails": status.consecutive_fails,
                    "inflight": status.inflight,
                    "model_cooldown": model_cooldown,
                })
            })
            .collect()
    }

    /// 供测试与排障：某账号的（模型级）冷却截止时间。
    pub fn cooldown_until(&self, account_id: &str) -> u64 {
        self.inner.lock().unwrap().status.get(account_id).map(|status| status.cooldown_until).unwrap_or(0)
    }

    pub fn model_cooldown_until(&self, account_id: &str, model: &str) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .status
            .get(account_id)
            .and_then(|status| status.model_cooldown.get(model).copied())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(accounts: &str, state: Value) -> (tempfile::TempDir, Arc<StateStore>, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let accounts_path = root.join("accounts.json");
        std::fs::write(&accounts_path, accounts).unwrap();
        let state_path = root.join("state.json");
        std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();
        let store = Arc::new(StateStore::open(&state_path).unwrap());
        (dir, store, accounts_path)
    }

    const TWO: &str = r#"[
        {"id":"a1","uid":"u1","variant":"cn","domain":"www.codebuddy.cn","access_token":"t1"},
        {"id":"a2","uid":"u2","variant":"cn","domain":"www.codebuddy.cn","access_token":"t2"}
    ]"#;

    #[test]
    fn round_robin_alternates() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let (_dir, store, path) = fixture(TWO, json!({"pool": {"routing": "round_robin"}}));
        let pool = Pool::new(store, path).with_clock(move || c.load(Ordering::Relaxed));
        let first = pool.select("m", None, None).unwrap();
        let second = pool.select("m", None, None).unwrap();
        let third = pool.select("m", None, None).unwrap();
        assert_eq!(first.account_id, "u1");
        assert_eq!(second.account_id, "u2");
        assert_eq!(third.account_id, "u1");
    }

    #[test]
    fn manual_routing_uses_active_account() {
        let (_dir, store, path) = fixture(TWO, json!({"pool": {"routing": "manual"}, "active": "u2"}));
        let pool = Pool::new(store, path);
        assert_eq!(pool.select("m", None, None).unwrap().account_id, "u2");
    }

    #[test]
    fn account_cooldown_is_skipped() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let (_dir, store, path) = fixture(TWO, json!({"pool": {"routing": "round_robin"}}));
        let pool = Pool::new(store, path).with_clock(move || c.load(Ordering::Relaxed));
        let failure = errors::classify(401, "", None);
        pool.record("u1", 401, "m", Some(&failure));
        assert_eq!(pool.select("m", None, None).unwrap().account_id, "u2");
        clock.store(2_000, Ordering::Relaxed);   // 冷却过期后重新可选
        let picked = pool.select("m", None, None).unwrap();
        assert!(picked.account_id == "u1" || picked.account_id == "u2");
    }

    #[test]
    fn model_cooldown_only_blocks_that_model() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let (_dir, store, path) = fixture(TWO, json!({"pool": {"routing": "manual"}, "active": "u1"}));
        let pool = Pool::new(store, path).with_clock(move || c.load(Ordering::Relaxed));
        let failure = errors::classify(400, r#"{"code":11134}"#, None);
        pool.record("u1", 429, "glm-5.3", Some(&failure));
        assert!(pool.select("glm-5.3", None, None).is_err(), "该模型应被冷却");
        assert_eq!(pool.select("deepseek-v4-flash", None, None).unwrap().account_id, "u1");
    }

    #[test]
    fn affinity_sticks_to_account() {
        let (_dir, store, path) = fixture(TWO, json!({"pool": {"routing": "round_robin"}}));
        let pool = Pool::new(store, path);
        let first = pool.select("m", Some("sess-1"), None).unwrap();
        let second = pool.select("m", Some("sess-1"), None).unwrap();
        assert_eq!(first.account_id, second.account_id);
    }

    #[test]
    fn paused_account_is_excluded() {
        let (_dir, store, path) = fixture(TWO, json!({
            "pool": {"routing": "manual"}, "active": "u1",
            "account_overrides": {"u1": {"enabled": false}}
        }));
        let pool = Pool::new(store, path);
        assert!(pool.select("m", None, None).is_err());
    }

    #[test]
    fn rows_expose_pool_state_and_last_error() {
        let clock = Arc::new(AtomicU64::new(1_000));
        let c = clock.clone();
        let (dir, store, path) = fixture(TWO, json!({"pool": {"routing": "round_robin"}}));
        let pool = Pool::new(store, path).with_clock(move || c.load(Ordering::Relaxed));
        let failure = errors::classify(401, r#"{"msg":"token expired"}"#, None);
        pool.record("u1", 401, "m", Some(&failure));

        let views = accounts::list_at(&dir.path().join("accounts.json"));
        let rows = pool.rows(&views);
        let row = rows.iter().find(|row| row["uid"] == json!("u1")).unwrap();
        assert_eq!(row["pool_state"], json!("cooling"));
        assert!(row["last_error"].as_str().unwrap().contains("token expired"));
        assert!(row["cooldown_until"].as_u64().unwrap() > 1_000);
    }

    #[test]
    fn rows_show_cooling_paused_and_exhausted() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let accounts_json = r#"[
            {"id":"a1","uid":"u1","variant":"cn","access_token":"t1"},
            {"id":"a2","uid":"u2","variant":"cn","access_token":"t2"},
            {"id":"a3","uid":"u3","variant":"cn","access_token":"t3"}
        ]"#;
        let (_dir, store, path) = fixture(accounts_json, json!({
            "pool": {"routing": "round_robin"},
            "account_overrides": {"u2": {"enabled": false}},
            "account_status": {"u3": {"remaining": 0, "credits_updated": now}}
        }));
        let pool = Pool::new(store, path);
        let views = accounts::list_at(&_dir.path().join("accounts.json"));
        let rows = pool.rows(&views);
        let state_of = |uid: &str| rows.iter().find(|row| row["uid"] == json!(uid)).unwrap()["pool_state"].clone();
        assert_eq!(state_of("u1"), json!("available"));
        assert_eq!(state_of("u2"), json!("paused"));
        assert_eq!(state_of("u3"), json!("exhausted"));
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn exhausted_credits_are_skipped() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let (_dir, store, path) = fixture(TWO, json!({
            "pool": {"routing": "round_robin"},
            "account_status": {"u1": {"remaining": 0, "credits_updated": now}}
        }));
        let pool = Pool::new(store, path);
        assert_eq!(pool.select("m", None, None).unwrap().account_id, "u2");
    }
}
