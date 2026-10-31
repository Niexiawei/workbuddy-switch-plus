//! 任务执行需要的外部依赖（账号来源、旅行缓存、令牌刷新回写）。
//!
//! **为什么要这一层**：旅行与签到的协议（怎么发、怎么判、失败怎么归类）只有一份，
//! 在 `travel.rs` / `checkin.rs` 里。但这两个模块原先的入口都是「读写桌面端账号库 +
//! 桌面端 `travel_cache.json`」的全局函数，没有注入点——反代想复用就只能另写一份，
//! 而两份协议迟早漂移（同一个账号在桌面端显示「旅行中」、在反代报别的错）。
//!
//! 这里把「数据从哪来、写回哪去」抽成一个 trait：
//!
//! - 桌面端实现 [`DesktopStore`]：账号库 `~/.wb-switch-plus/accounts.json`，
//!   缓存 `travel_cache.json` —— 与改造前逐字一致；
//! - 反代实现由 `wb-switch-proxy` 提供：账号库走它自己的 `ProxyConfig.accounts_path`，
//!   旅行缓存落 `state.json` 的 `travel_states`。
//!
//! **状态机不在这里**：本文件只管数据进出，`travel.rs` 那 1300 行一行没改。

use serde_json::{json, Value};

use crate::modules::account::{self, load_accounts};
use crate::modules::config::{load_travel_cache, now_ms, save_travel_cache};

/// 本地日期（`YYYY-MM-DD`）。与 `travel.rs::today_str` 同口径：
/// 用 `Local` 是因为桌面端的缓存文件本来就按本机日期滚动，改时区会让存量缓存对不上。
fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 一次任务执行所需的外部依赖。
///
/// 实现方只负责「取/存」，不参与任何业务判断——判断留给 `travel.rs` / `checkin.rs`，
/// 这样两个宿主对同一个账号才会给出同一个结论。
pub trait TaskStore: Send + Sync {
    /// 该账号当前的账号记录（形如 `accounts.json` 的一条）。
    ///
    /// `account_id` 可能是账号库的 `id`，也可能是 `uid`——调用方（反代）用池里的
    /// `id` 作为主键，而桌面端用 `uid`，两种都可能传进来。
    fn account(&self, account_id: &str) -> Option<Value>;

    /// 读该账号上一次的旅行结果。没有记录时返回空对象。
    fn travel_entry(&self, account_id: &str) -> Value;

    /// 写回一次旅行结果（整条覆盖，调用方已合并好）。
    fn save_travel_entry(&self, account_id: &str, entry: Value) -> std::io::Result<()>;

    /// 令牌刷新后的落点。
    ///
    /// 默认空实现 =「只留在内存里」。反代与桌面端都不写账号库（`WB_SWITCH_WRITE_BACK`
    /// 那条口子是独立议题），所以默认行为就是正确行为；留着这个钩子是为了让
    /// 以后要回写的宿主有一个明确的挂点，而不是又去改 `refresh.rs`。
    fn store_refreshed(&self, _account_id: &str, _refreshed: &Value) {}

    /// 该账号是否支持成长中心（派猫猫旅行）。默认按账号档位判断。
    fn supports_travel(&self, account: &Value) -> bool {
        crate::modules::account::variant_of(account).supports_travel()
    }
}

/// 桌面端实现：全局账号库 + `travel_cache.json`。
///
/// 逐字沿用改造前的全局函数，只是把它们包进 trait——`run_travel_cycle` 等既有入口
/// 仍然走同一条路径，所以桌面端行为零变化。
pub struct DesktopStore;

impl TaskStore for DesktopStore {
    fn account(&self, account_id: &str) -> Option<Value> {
        account::find_account(account_id)
            .or_else(|| {
                // `find_account` 只认 id / uid 精确匹配；反代还会传池里的 id。
                // 这里补一条「按 id 再找一次」的兜底，成本可忽略。
                load_accounts().into_iter().find(|row| {
                    row.get("id").and_then(Value::as_str) == Some(account_id)
                        || row.get("uid").and_then(Value::as_str) == Some(account_id)
                })
            })
            .filter(|value| !value.is_null())
    }

    fn travel_entry(&self, account_id: &str) -> Value {
        let cache = load_travel_cache();
        // 缓存按 accountId 键存；历史上还写过 uid 键（早于 account_key 统一），
        // 两种都认，免得老用户的行程在升级后「消失」。
        cache
            .get("results")
            .and_then(Value::as_object)
            .and_then(|results| {
                results
                    .get(account_id)
                    .or_else(|| {
                        let uid = account::find_account(account_id)
                            .and_then(|acc| acc.get("uid").and_then(Value::as_str).map(String::from));
                        uid.and_then(|uid| results.get(&uid))
                    })
                    .cloned()
            })
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    fn save_travel_entry(&self, account_id: &str, entry: Value) -> std::io::Result<()> {
        // 读-改-写整份缓存：与 `write_travel_overlay` 同一把锁，避免与自动派发循环打架。
        crate::modules::config::with_travel_cache_lock(|| {
            let mut disk = load_travel_cache();
            let today = today_str();
            let results = disk
                .as_object_mut()
                .map(|object| {
                    object
                        .entry("results".to_string())
                        .or_insert_with(|| json!({}))
                })
                .and_then(Value::as_object_mut);
            match results {
                Some(results) => {
                    results.insert(account_id.to_string(), entry);
                }
                None => {
                    disk["results"] = json!({ account_id: entry });
                }
            }
            disk["date"] = json!(today);
            // `updated` 只是排障用的时间戳，写入方不依赖它做判断。
            disk["updated"] = json!(now_ms() / 1000);
            save_travel_cache(&disk)
        })
    }
}
