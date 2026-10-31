//! 账号池的「动作」层：单个账号的执行、批量编排、任务流水与结果记账。
//!
//! 对应 Python 的 `reverse-proxy/admin/pool.py::operate` / `batch` / `log_task`。
//! `admin.rs` 只做参数与鉴权，真正的活儿全部在这里——这样「手动点一下」与
//! 「批量跑一遍」走的是同一段代码，不会出现两边对同一个账号给出不同结论。
//!
//! 三条不变式：
//!
//! 1. **协议只有一份**：旅行走 `wb_switch_core::modules::travel`（传 [`ProxyTaskStore`]），
//!    签到走 `checkin`，活跃上报走 [`crate::report`]。这里只做编排。
//! 2. **单账号串行**：同一账号同时只允许一个动作在跑（Python 是 `operation_lock(aid)`）。
//!    拿不到锁就立刻返回可读文案，**不排队**——排队会让批量任务卡在一个人工点击后面。
//! 3. **批量里单个账号失败不掀翻整批**：每个账号的结果都折成 `{id, ok, message}`
//!    （HTTP 永远 200），失败信息进 `results[]`，前端据此逐行显示。
//!
//! 落盘边界：只写 `state.json`（`account_status` / `travel_states` / `checkin_logs`），
//! 账号库始终只读。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use wb_switch_core::modules::account::{self, build_auth_headers, variant_of};
use wb_switch_core::modules::checkin;
use wb_switch_core::modules::config::now_secs;
use wb_switch_core::modules::task_store::TaskStore;
use wb_switch_core::modules::travel;
use wb_switch_core::modules::variant::WbVariant;

use crate::report;
use crate::state::StateStore;

/// 任务流水上限（与 Python `CHECKIN_LOG_LIMIT` 同值）。
pub const CHECKIN_LOG_LIMIT: usize = 500;
/// 流水里 message 的截断长度。
const MESSAGE_LIMIT: usize = 200;
/// 批量并发度（Python 是 `asyncio.Semaphore(3)`）。
pub const BATCH_CONCURRENCY: usize = 3;

/// 能被执行的动作。与 `admin.rs` 或前端对不上时**在这里**报错，
/// 而不是让它掉进一个「尚未迁移」的兜底分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Checkin,
    Refresh,
    Travel,
    TravelClaim,
    Active,
}

impl Action {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "status" => Some(Self::Status),
            "checkin" => Some(Self::Checkin),
            "refresh" => Some(Self::Refresh),
            "travel" => Some(Self::Travel),
            "travel-claim" => Some(Self::TravelClaim),
            "active" => Some(Self::Active),
            _ => None,
        }
    }

    /// 落流水时用的类别。`None` = 这个动作不进流水（查积分、刷新凭据不是「任务」）。
    fn kind(self) -> Option<&'static str> {
        match self {
            Self::Checkin => Some("checkin"),
            Self::Travel | Self::TravelClaim => Some("travel"),
            Self::Active => Some("active"),
            Self::Status | Self::Refresh => None,
        }
    }
}

/// 动作来源：用户手动点，还是批量调度。落进流水的 `source` 字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Manual,
    Batch,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Batch => "batch",
        }
    }
}

/// 反代侧的 [`TaskStore`] 实现。
///
/// - 账号来自 `cfg.accounts_path`（只读）；
/// - 旅行缓存落 `state.json` 的 `travel_states[aid]`，**不碰桌面端的 `travel_cache.json`**
///   ——两套缓存同源会打架（桌面端后台循环也在写，且它的 `completed` 语义是
///   「今日全部完成」，反代只关心单个账号）。
pub struct ProxyTaskStore<'a> {
    store: &'a StateStore,
    accounts_path: &'a Path,
}

impl<'a> ProxyTaskStore<'a> {
    pub fn new(store: &'a StateStore, accounts_path: &'a Path) -> Self {
        Self {
            store,
            accounts_path,
        }
    }
}

impl TaskStore for ProxyTaskStore<'_> {
    fn account(&self, account_id: &str) -> Option<Value> {
        account::load_accounts_at(self.accounts_path)
            .into_iter()
            .find(|row| {
                row.get("id").and_then(Value::as_str) == Some(account_id)
                    || row.get("uid").and_then(Value::as_str) == Some(account_id)
            })
    }

    fn travel_entry(&self, account_id: &str) -> Value {
        self.store
            .get("travel_states")
            .and_then(|value| value.get(account_id).cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    fn save_travel_entry(&self, account_id: &str, entry: Value) -> std::io::Result<()> {
        // `date` / `updated` 由这里补，不在调用方：Python 的 `set_travel_state`
        // 也是这么做的，而 `rows()` 会拿 `date` 判「是不是今天的」。
        self.store.update(|value| {
            let states = value
                .as_object_mut()
                .map(|object| {
                    object
                        .entry("travel_states".to_string())
                        .or_insert_with(|| json!({}))
                })
                .and_then(Value::as_object_mut);
            let Some(states) = states else { return };
            let mut entry = entry;
            if let Some(object) = entry.as_object_mut() {
                object.insert("date".into(), json!(cn_date()));
                object.insert("updated".into(), json!(now_secs()));
            }
            states.insert(account_id.to_string(), entry);
        })
    }
}

/// 账号池动作执行器。
pub struct TaskRunner {
    store: Arc<StateStore>,
    accounts_path: std::path::PathBuf,
    /// 上游基址（积分 / 签到 / 刷新都走它）。
    ///
    /// 必须**跟着宿主的 `ProxyConfig` 走**，不能用 `ProxyConfig::for_user()` 现建：
    /// 那样测试注入的 `billing_base` 会被丢掉，真实的「查积分」也会绕过用户配置
    /// （例如自建网关 / 代理）。这是 `account_status_reads_billing_credits` 抓到的。
    billing_base: String,
    /// 批量任务互斥（一把锁）：已经有批量在跑时再点一次 → 409，而不是排队。
    jobs: Arc<Semaphore>,
    /// 单账号串行锁。与 `jobs` 不同：它是**按账号**的，不影响别的账号。
    per_account: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
}

/// 一次动作的结果。`ok=false` 不抛错——调用方（前端）拿到可读文案自己展示。
pub type ActionResult = Value;

impl TaskRunner {
    pub fn new(store: Arc<StateStore>, accounts_path: std::path::PathBuf) -> Self {
        Self::with_billing_base(store, accounts_path, crate::config::DEFAULT_BILLING.to_string())
    }

    pub fn with_billing_base(
        store: Arc<StateStore>,
        accounts_path: std::path::PathBuf,
        billing_base: impl Into<String>,
    ) -> Self {
        Self {
            store,
            accounts_path,
            billing_base: billing_base.into(),
            jobs: Arc::new(Semaphore::new(1)),
            per_account: Mutex::new(HashMap::new()),
            clock: Box::new(now_secs),
        }
    }

    /// 动作层用的上游配置。只覆盖它真正依赖的两个字段，其余沿用环境默认。
    fn cfg(&self) -> crate::config::ProxyConfig {
        crate::config::ProxyConfig::for_user()
            .with_accounts(self.accounts_path.clone())
            .with_billing_base(self.billing_base.clone())
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn account_lock(&self, aid: &str) -> Arc<AsyncMutex<()>> {
        let mut guard = self.per_account.lock().unwrap();
        guard
            .entry(aid.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub fn accounts_path(&self) -> &Path {
        &self.accounts_path
    }

    fn task_store(&self) -> ProxyTaskStore<'_> {
        ProxyTaskStore::new(&self.store, &self.accounts_path)
    }

    /// 执行单个动作。**不会**因为账号忙而等待：拿不到锁就返回一条可读结果。
    pub async fn operate(&self, aid: &str, action: Action, source: Source) -> ActionResult {
        let record = match self.task_store().account(aid) {
            Some(record) => record,
            None => return json!({"id": aid, "ok": false, "message": "账号不存在"}),
        };
        // 暂停的账号不执行任何动作（Python `_operate` 的第一句就是它）。
        if self.is_paused(aid) {
            return json!({"id": aid, "ok": false, "message": "账号已暂停，未执行操作"});
        }

        let lock = self.account_lock(aid);
        let Ok(_guard) = lock.try_lock() else {
            return json!({"id": aid, "ok": false, "message": "该账号正在执行其它任务，请稍后"});
        };

        let mut result = match action {
            Action::Status => self.status(&record).await,
            Action::Checkin => self.checkin(&record).await,
            Action::Refresh => self.refresh(aid, &record).await,
            Action::Travel => self.travel(&record).await,
            Action::TravelClaim => self.travel_claim(&record).await,
            Action::Active => self.active(&record).await,
        };
        // 结果形状统一：调用方要 `id`，前端要 `message`，批量要 `ok`。
        if let Some(object) = result.as_object_mut() {
            object.insert("id".to_string(), json!(aid));
            object
                .entry("ok".to_string())
                .or_insert_with(|| json!(false));
            object
                .entry("message".to_string())
                .or_insert_with(|| json!(""));
        } else {
            result = json!({"id": aid, "ok": false, "message": result.to_string()});
        }

        if let Some(kind) = action.kind() {
            let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let message = result
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let credits = result.get("rewardCredit").cloned();
            self.log_task(aid, kind, ok, &message, source, credits);
        }
        result
    }

    /// 批量执行。拿不到 jobs 锁时返回 `None`（HTTP 层转成 409）。
    pub async fn batch(&self, action: Action) -> Option<Value> {
        let Ok(jobs) = self.jobs.clone().try_acquire_owned() else {
            return None;
        };
        let ids: Vec<String> = account::load_accounts_at(&self.accounts_path)
            .into_iter()
            .filter_map(|row| {
                let aid = row
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .or_else(|| row.get("uid").and_then(Value::as_str))?
                    .to_string();
                // 暂停的账号不参与批量：与单个动作同一判据。
                (!self.is_paused(&aid)).then_some(aid)
            })
            .collect();

        let limit = Arc::new(Semaphore::new(BATCH_CONCURRENCY));
        let results = join_all(ids.into_iter().map(|aid| {
            let limit = limit.clone();
            async move {
                let _permit = limit.acquire().await;
                self.operate(&aid, action, Source::Batch).await
            }
        }))
        .await;
        drop(jobs);
        Some(json!({"results": results}))
    }

    fn is_paused(&self, aid: &str) -> bool {
        self.store
            .get("account_overrides")
            .and_then(|value| value.get(aid).cloned())
            .and_then(|row| row.get("enabled").and_then(Value::as_bool))
            == Some(false)
    }

    // ── 各动作 ────────────────────────────────────────────────────────

    async fn status(&self, record: &Value) -> Value {
        let token = plain_token(record, "access_token");
        if token.is_empty() {
            return json!({"ok": false, "message": token_error(record)});
        }
        // 积分走 billing 接口（`/v2/billing/meter/get-user-resource`）：
        // 它是唯一给出 `remaining` / `packages` 的来源。签到状态接口不带这两个字段，
        // 所以不能拿它顶替——那样界面上「剩余积分」会永远是「—」。
        match crate::billing::credits(&self.cfg(), &token).await {
            Ok(credits) => {
                let remaining = credits.get("remaining").cloned().unwrap_or(Value::Null);
                let packages = credits.get("packages").cloned().unwrap_or(Value::Null);
                // 一次成功的余额查询就是「当前凭据有效」的证明：顺手清掉失效标记
                // 与冷却，和 Python `_operate` 的 `auth_invalid=False` 同口径。
                self.update_status(record, |row| {
                    row.insert("remaining".into(), remaining.clone());
                    row.insert("packages".into(), packages.clone());
                    row.insert("auth_invalid".into(), json!(false));
                    row.insert("credits_updated".into(), json!(self.now()));
                });
                json!({
                    "ok": true,
                    "message": "积分状态已更新",
                    "remaining": remaining,
                    "packages": packages,
                })
            }
            Err(error) => {
                self.update_status(record, |row| {
                    row.insert("last_error".into(), json!(error));
                });
                json!({"ok": false, "message": error})
            }
        }
    }

    async fn checkin(&self, record: &Value) -> Value {
        // 国际版没有签到体系：**先判断再决定发不发**（与 core 同一判据）。
        let variant = variant_of(record);
        if !variant.supports_checkin() {
            return json!({"ok": true, "message": "该版本没有签到功能，已跳过"});
        }
        let result = checkin::checkin_account(record).await;
        match result.get("result").and_then(Value::as_str) {
            Some("already") => {
                self.update_status(record, |row| {
                    row.insert("checkin_date".into(), json!(cn_date()));
                });
                json!({"ok": true, "message": "今日已签到，无需重复签到"})
            }
            Some("success") | Some("done") => {
                self.update_status(record, |row| {
                    row.insert("checkin_date".into(), json!(cn_date()));
                    row.insert("cooldown_until".into(), json!(0));
                });
                json!({"ok": true, "message": "签到成功"})
            }
            Some("skipped") => json!({"ok": true, "message": "该版本没有签到功能，已跳过"}),
            _ => {
                let message = result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("签到失败")
                    .to_string();
                self.update_status(record, |row| {
                    row.insert("last_error".into(), json!(message));
                });
                json!({"ok": false, "message": message})
            }
        }
    }

    async fn refresh(&self, aid: &str, record: &Value) -> Value {
        let refresh_token = plain_token(record, "refresh_token");
        if refresh_token.is_empty() {
            return json!({"ok": false, "message": "账号没有 refresh token，需重新登录"});
        }
        match crate::billing::refresh_token(&self.cfg(), &refresh_token).await {
            Ok(_) => {
                // 落盘到 state（**不写账号库**）：不落的话界面上点了「刷新凭据」
                // 没有任何反馈，用户会以为按钮坏了。
                self.update_status_for(aid, |row| {
                    row.insert("token_refreshed".into(), json!(self.now()));
                    row.insert("auth_invalid".into(), json!(false));
                    row.insert("cooldown_until".into(), json!(0));
                    row.insert("last_error".into(), Value::Null);
                });
                json!({"ok": true, "message": "登录凭据已刷新"})
            }
            Err(error) => {
                self.update_status(record, |row| {
                    row.insert("last_error".into(), json!(error));
                });
                json!({"ok": false, "message": error})
            }
        }
    }

    async fn travel(&self, record: &Value) -> Value {
        // 国际版没有成长中心：拦在这里，别让调度对着必然失败的端点反复打。
        if !variant_of(record).supports_travel() {
            return json!({"ok": false, "message": "国际版账号没有成长中心，已跳过"});
        }
        let store = self.task_store();
        let aid = record_key(record);
        let result = travel::depart_travel_with_store(&store, record).await;
        // core 的结果写回缓存（`claimedAt` / `arriveAt` 这些跨轮次字段靠它保住）。
        let _ = store.save_travel_entry(&aid, result.clone());
        json!({
            "ok": result.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "message": result.get("message").and_then(Value::as_str).unwrap_or(""),
            "state": result.get("state").cloned().unwrap_or(Value::Null),
            "skip": result.get("skip").cloned().unwrap_or(Value::Null),
            "locationName": result.get("locationName").cloned().unwrap_or(Value::Null),
            "rewardCredit": result.get("rewardCredit").cloned().unwrap_or(Value::Null),
        })
    }

    async fn travel_claim(&self, record: &Value) -> Value {
        if !variant_of(record).supports_travel() {
            return json!({"ok": false, "message": "国际版账号没有成长中心，已跳过"});
        }
        let store = self.task_store();
        let aid = record_key(record);
        let prior = store.travel_entry(&aid);
        let prior_value = (!prior.as_object().map(|o| o.is_empty()).unwrap_or(true)).then_some(&prior);
        // 走 sync 而不是纯 claim：账号其实还没派过的时候，它会把这一次变成派发，
        // 而不是对着一个 idle 账号空领一次（Python 的 travel-claim 也是 sync）。
        let result = travel::sync_for_dispatch_with_store(&store, record, prior_value).await;
        let _ = store.save_travel_entry(&aid, result.clone());
        json!({
            "ok": result.get("ok").and_then(Value::as_bool).unwrap_or(false),
            "message": result.get("message").and_then(Value::as_str).unwrap_or(""),
            "state": result.get("state").cloned().unwrap_or(Value::Null),
            "skip": result.get("skip").cloned().unwrap_or(Value::Null),
            "claimed": result.get("claimed").cloned().unwrap_or(Value::Null),
            "rewardCredit": result.get("rewardCredit").cloned().unwrap_or(Value::Null),
        })
    }

    async fn active(&self, record: &Value) -> Value {
        let headers: HashMap<String, String> = build_auth_headers(record);
        // 国际版走另一个计费域，这里发的还是国内主机，必然失败。
        // 记成当天有效的终局跳过（而不是普通失败）：这一类重试一万次也不会变好。
        if headers
            .get("X-Domain")
            .map(|domain| domain.to_lowercase().contains("workbuddy.ai"))
            .unwrap_or(false)
        {
            self.note_active(record, ActiveOutcome::TerminalSkip("intl"), "当前仅支持国内账号，已跳过");
            return json!({"ok": false, "sent": 0, "skip": "intl", "message": "当前仅支持国内账号，已跳过"});
        }

        let count = report::clamp_count(self.setting("active_reports", report::DEFAULT_REPORTS));
        let result = report::report(&headers, count).await;
        let sent = result.get("sent").and_then(Value::as_i64).unwrap_or(0);
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            let message = result
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("活跃上报失败")
                .to_string();
            self.note_active(record, ActiveOutcome::Failure, &message);
            return json!({"ok": false, "sent": sent, "message": message});
        }

        // 回读连登天数自检：上报 200 不等于生效，days=0 就是被静默丢弃了。
        // 探针本身读不出来时 ok=false —— 那是「探不了」，不是「没生效」，
        // 不能反过来把已经成功的上报判死。
        let check = report::streak(&headers).await;
        let streak_ok = check.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let days = check.get("days").and_then(Value::as_i64).unwrap_or(0);
        let message = if !streak_ok {
            self.note_active(record, ActiveOutcome::Unverified, "");
            format!("活跃上报完成（{sent} 条），连登状态未能确认")
        } else if days > 0 {
            self.note_active(record, ActiveOutcome::Verified(days), "");
            format!("活跃上报完成（{sent} 条），连登 {days} 天")
        } else {
            // 「上报 200 但被静默丢弃」：**不能**关闸门，当天还要按节拍重试。
            self.note_active(record, ActiveOutcome::Unverified, "");
            format!("已上报 {sent} 条，但连登天数仍为 0（可能被上游丢弃）")
        };
        json!({"ok": true, "sent": sent, "streak": days, "streak_ok": streak_ok, "message": message})
    }

    /// 记一次活跃上报的尝试。**「报上去了」与「当天结束了」是两件事。**
    fn note_active(&self, record: &Value, outcome: ActiveOutcome, detail: &str) {
        let today = cn_date();
        let aid = record_key(record);
        self.update_status_for(&aid, |row| {
            row.insert("auto_active_attempt".into(), json!(self.now()));
            match outcome {
                ActiveOutcome::Verified(days) => {
                    row.insert("active_date".into(), json!(today));
                    row.insert("active_streak".into(), json!(days));
                    // 成功就把上一次的失败原因清掉：留着会让卡片上永远挂着一条
                    // 「已经解决了的」报错，比没有状态更误导人。
                    row.insert("active_error".into(), Value::Null);
                }
                ActiveOutcome::Unverified => {
                    row.insert(
                        "active_error".into(),
                        json!(if detail.is_empty() {
                            "已上报，但连登状态未能确认"
                        } else {
                            detail
                        }),
                    );
                }
                ActiveOutcome::TerminalSkip(skip) => {
                    row.insert("active_skip".into(), json!(skip));
                    row.insert("active_skip_date".into(), json!(today));
                    row.insert("active_error".into(), json!(detail));
                }
                ActiveOutcome::Failure => {
                    if !detail.is_empty() {
                        row.insert("active_error".into(), json!(detail));
                    }
                }
            }
        });
    }

    /// 读 `pool.<key>` 并归一成整数。
    ///
    /// 三种形态都要认，因为它们真的都会出现：JSON 整数（`3`）、浮点（前端
    /// `Number()` 可能给出 `3.0`）、以及数字字符串（老 state.json / 手改）。
    /// 只认 `as_i64` 会让「活跃上报条数」静默退回默认 1，而界面上那个输入框
    /// 显示的是用户刚填的值——这种不一致最容易被当成「设置没保存」。
    fn setting(&self, key: &str, fallback: i64) -> i64 {
        self.store
            .get("pool")
            .and_then(|pool| pool.get(key).cloned())
            .and_then(|value| match value {
                Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|f| f as i64)),
                Value::String(text) => text.trim().parse::<f64>().ok().map(|f| f as i64),
                Value::Bool(flag) => Some(i64::from(flag)),
                _ => None,
            })
            .unwrap_or(fallback)
    }

    fn update_status(&self, record: &Value, apply: impl FnOnce(&mut serde_json::Map<String, Value>)) {
        let aid = record_key(record);
        self.update_status_for(&aid, apply);
    }

    fn update_status_for(
        &self,
        aid: &str,
        apply: impl FnOnce(&mut serde_json::Map<String, Value>),
    ) {
        let _ = self.store.update(|value| {
            let Some(status) = value
                .as_object_mut()
                .map(|object| {
                    object
                        .entry("account_status".to_string())
                        .or_insert_with(|| json!({}))
                })
                .and_then(Value::as_object_mut)
            else {
                return;
            };
            let entry = status
                .entry(aid.to_string())
                .or_insert_with(|| json!({}));
            let Some(object) = entry.as_object_mut() else {
                return;
            };
            apply(object);
        });
    }

    /// 追加一条任务流水。成功失败都记——出了问题（今天到底签没签、哪一步报的错）
    /// 要能回溯，只存一个日期字符串是不够的。
    pub fn log_task(
        &self,
        aid: &str,
        kind: &str,
        ok: bool,
        message: &str,
        source: Source,
        credits: Option<Value>,
    ) {
        let name = self
            .task_store()
            .account(aid)
            .map(|record| account::account_display_name(&record))
            .unwrap_or_else(|| aid.to_string());
        let _ = self.store.update(|value| {
            let Some(logs) = value
                .as_object_mut()
                .map(|object| {
                    object
                        .entry("checkin_logs".to_string())
                        .or_insert_with(|| json!([]))
                })
                .and_then(Value::as_array_mut)
            else {
                return;
            };
            let mut entry = json!({
                "time": now_secs(),
                "id": aid,
                "name": name,
                "kind": kind,
                "ok": ok,
                "message": message.chars().take(MESSAGE_LIMIT).collect::<String>(),
                "source": source.as_str(),
            });
            if let Some(credits) = credits.filter(|value| !value.is_null()) {
                entry["credits"] = credits;
            }
            logs.push(entry);
            // 上限 500 条：只留最新的，旧的直接丢（Python 也是 `del logs[:n]`）。
            if logs.len() > CHECKIN_LOG_LIMIT {
                let drop_count = logs.len() - CHECKIN_LOG_LIMIT;
                logs.drain(0..drop_count);
            }
        });
    }

    /// 任务流水，**新的在前**。`kind` 为空时返回全部类型。
    pub fn task_logs(&self, limit: usize, kind: Option<&str>) -> Vec<Value> {
        let mut logs: Vec<Value> = self
            .store
            .get("checkin_logs")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|row| match kind {
                Some(kind) => row.get("kind").and_then(Value::as_str).unwrap_or("checkin") == kind,
                None => true,
            })
            .collect();
        logs.reverse();
        logs.truncate(limit.clamp(1, CHECKIN_LOG_LIMIT));
        logs
    }

    /// 按类别汇总：每类成功/失败多少、最近一次是什么时候。
    pub fn task_summary(&self, hours: i64) -> Value {
        let window = hours.max(1);
        let since = self.now() - window * 3600;
        let logs = self
            .store
            .get("checkin_logs")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        let mut rows: Vec<Value> = Vec::new();
        for (kind, total, ok, failed, last) in tally(&logs, since) {
            rows.push(json!({
                "kind": kind,
                "total": total,
                "ok": ok,
                "failed": failed,
                "last": last,
                "success_rate": if total > 0 { (ok as f64 / total as f64 * 10000.0).round() / 10000.0 } else { 0.0 },
            }));
        }
        json!({"hours": window, "kinds": rows})
    }
}

#[derive(Debug, Clone, Copy)]
enum ActiveOutcome {
    /// 有正面证据：回读成功且天数 > 0。
    Verified(i64),
    /// 报到服务端了，但没能证明生效（探到 0 / 探针坏了）。
    Unverified,
    /// 当天有效的终局跳过（目前只有国际版）。
    TerminalSkip(&'static str),
    Failure,
}

/// 按 kind 聚合时间窗内的流水。返回 `(kind, total, ok, failed, last)`，顺序按 `TASK_KINDS`。
fn tally(logs: &[Value], since: i64) -> Vec<(&'static str, i64, i64, i64, Option<i64>)> {
    const KINDS: [&str; 4] = ["checkin", "travel", "keepalive", "active"];
    let mut slots: Vec<(&'static str, i64, i64, i64, Option<i64>)> =
        KINDS.iter().map(|kind| (*kind, 0, 0, 0, None)).collect();
    for row in logs {
        let time = row.get("time").and_then(Value::as_i64).unwrap_or(0);
        if time < since {
            continue;
        }
        let raw_kind = row.get("kind").and_then(Value::as_str).unwrap_or("checkin");
        let index = match slots.iter().position(|slot| slot.0 == raw_kind) {
            Some(index) => index,
            // 未来新增的类别也不会被吞掉。
            None => {
                let leaked: &'static str = Box::leak(raw_kind.to_string().into_boxed_str());
                slots.push((leaked, 0, 0, 0, None));
                slots.len() - 1
            }
        };
        let slot = &mut slots[index];
        slot.1 += 1;
        if row.get("ok").and_then(Value::as_bool) == Some(true) {
            slot.2 += 1;
        } else {
            slot.3 += 1;
        }
        slot.4 = Some(slot.4.unwrap_or(0).max(time));
    }
    slots
}

fn account_plain(record: &Value, key: &str) -> String {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(|token| token.trim().to_string())
        .unwrap_or_default()
}

fn plain_token(record: &Value, key: &str) -> String {
    account_plain(record, key)
}

/// 凭据不可用时的可读说明。加密信封与「压根没 token」要分开：
/// 前者得去解密（或换一个没加密的账号），后者得重新登录。
fn token_error(record: &Value) -> String {
    match account::envelope_token_error(record) {
        Some(error) => error,
        None => "账号库里没有可用 token，需重新登录".to_string(),
    }
}

fn record_key(record: &Value) -> String {
    record
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| record.get("uid").and_then(Value::as_str))
        .map(String::from)
        .unwrap_or_default()
}

/// `Asia/Shanghai` 的日期字符串。
///
/// **不能用 `Local`**：Python 侧是固定 +8，用本地时区会在非 +8 机器上把
/// 「今日已签到」判错一天。
pub fn cn_date() -> String {
    cn_today_at(now_secs())
}

fn cn_today_at(secs: i64) -> String {
    use chrono::{FixedOffset, TimeZone};
    let offset = FixedOffset::east_opt(8 * 3600).expect("+08:00");
    offset
        .timestamp_opt(secs, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 供 `rows()` 复用：某账号是否「今天已签到」。
pub fn checked_in_today(status: &Value) -> bool {
    status.get("checkin_date").and_then(Value::as_str) == Some(cn_date().as_str())
}

/// 供 `rows()` 复用：旅行卡片的四档展示值（与桌面端 `display_label` 同口径）。
pub fn travel_label(entry: &Value) -> &'static str {
    let in_flight = entry.get("ok").and_then(Value::as_bool) == Some(true)
        && entry.get("claimed").and_then(Value::as_bool) != Some(true);
    if in_flight {
        return "traveling";
    }
    let same_day = entry.get("date").and_then(Value::as_str) == Some(cn_date().as_str());
    if same_day && entry.get("skip").and_then(Value::as_str) == Some("no-buddy") {
        return "no-buddy";
    }
    if same_day && entry.get("claimed").and_then(Value::as_bool) == Some(true) {
        return "finished";
    }
    "untraveled"
}

/// 账号档位 → 界面用的 realm 字符串（`cn` / `global`）。
pub fn realm_of(record: &Value) -> &'static str {
    match variant_of(record) {
        WbVariant::Cn => "cn",
        WbVariant::Ai => "global",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_parse_covers_every_frontend_action() {
        for (raw, expected) in [
            ("status", Action::Status),
            ("checkin", Action::Checkin),
            ("refresh", Action::Refresh),
            ("travel", Action::Travel),
            ("travel-claim", Action::TravelClaim),
            ("active", Action::Active),
        ] {
            assert_eq!(Action::parse(raw), Some(expected), "{raw} 必须被识别");
        }
        assert_eq!(Action::parse("unknown"), None);
    }

    #[test]
    fn only_task_actions_reach_the_ledger() {
        // 查积分与刷新凭据不是「任务」，不该往 500 条的流水里塞记录。
        assert_eq!(Action::Status.kind(), None);
        assert_eq!(Action::Refresh.kind(), None);
        assert_eq!(Action::Checkin.kind(), Some("checkin"));
        assert_eq!(Action::Travel.kind(), Some("travel"));
        assert_eq!(Action::TravelClaim.kind(), Some("travel"));
        assert_eq!(Action::Active.kind(), Some("active"));
    }

    #[test]
    fn travel_label_matches_the_desktop_four_states() {
        let today = cn_date();
        assert_eq!(travel_label(&json!({})), "untraveled");
        assert_eq!(
            travel_label(&json!({"ok": true, "claimed": false})),
            "traveling",
            "在途（未领奖）优先于当日判定"
        );
        assert_eq!(
            travel_label(&json!({"date": today, "ok": false, "skip": "no-buddy"})),
            "no-buddy"
        );
        assert_eq!(
            travel_label(&json!({"date": today, "ok": true, "claimed": true})),
            "finished"
        );
        // 跨日的已领记录不再显示「今日已派」。
        assert_eq!(
            travel_label(&json!({"date": "2000-01-01", "ok": true, "claimed": true})),
            "untraveled"
        );
    }

    #[test]
    fn checked_in_today_compares_against_china_date() {
        assert!(checked_in_today(&json!({"checkin_date": cn_date()})));
        assert!(!checked_in_today(&json!({"checkin_date": "2000-01-01"})));
        assert!(!checked_in_today(&json!({})));
    }

    #[test]
    fn setting_normalizes_numbers_floats_and_strings() {
        // 三种形态都会真的出现：JSON 整数、前端 Number() 给出的浮点、
        // 以及老 state.json / 手改留下的数字字符串。只认 as_i64 会让
        // 「活跃上报条数」静默退回默认 1，而输入框显示的是用户刚填的值。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"pool": {
                "int": 3, "float": 3.7, "string": "4", "bool": true, "junk": "abc"
            }}))
            .unwrap(),
        )
        .unwrap();
        let store = std::sync::Arc::new(crate::state::StateStore::open(&path).unwrap());
        let runner = TaskRunner::new(store, dir.path().join("accounts.json"));
        assert_eq!(runner.setting("int", 1), 3);
        assert_eq!(runner.setting("float", 1), 3, "浮点截断而不是退回默认");
        assert_eq!(runner.setting("string", 1), 4, "数字字符串也要认");
        assert_eq!(runner.setting("bool", 1), 1, "布尔按 0/1 归一");
        assert_eq!(runner.setting("junk", 7), 7, "读不出来用默认值");
        assert_eq!(runner.setting("missing", 9), 9);
    }
    #[test]
    fn tally_counts_per_kind_and_ignores_the_window() {
        let now = 1_800_000_000_i64;
        let logs = vec![
            json!({"time": now - 60, "kind": "checkin", "ok": true}),
            json!({"time": now - 30, "kind": "checkin", "ok": false}),
            json!({"time": now - 10, "kind": "travel", "ok": true}),
            // 窗口外的应当被丢弃。
            json!({"time": now - 10 * 3600, "kind": "active", "ok": true}),
        ];
        let rows = tally(&logs, now - 3600);
        let checkin = rows.iter().find(|row| row.0 == "checkin").unwrap();
        assert_eq!((checkin.1, checkin.2, checkin.3), (2, 1, 1));
        let travel = rows.iter().find(|row| row.0 == "travel").unwrap();
        assert_eq!((travel.1, travel.2), (1, 1));
        let active = rows.iter().find(|row| row.0 == "active").unwrap();
        assert_eq!(active.1, 0, "窗口外的记录不计数");
    }
}
