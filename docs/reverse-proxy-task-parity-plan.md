# 反代账号池对齐计划（v4）：批量任务 / 旅行动作 / 凭据列 —— 把 Rust 引擎补齐到 Python 口径

> 状态：**P-A0 / P-A1 / P-A2 已实施**（2026-10-31），P-A3–P-A5 待做。本文上方是口径与切分，§10 是落地记录。
> 承接：`docs/reverse-proxy-internal-plane-plan.md`（v3，管理面已进程内化，P0–P5 完成）。
> v3 解决了「管理面不再走 HTTP」；本文解决 v3 验收表里没被勾到的那一类遗留：
> **管理面路由齐全、但账号池的「动作」这半边仍停在占位实现**，界面照常渲染，点了就报「尚未迁移」。
> 约定：引用「v2」= `reverse-proxy-integration-plan.md`，引用「v3」= `reverse-proxy-internal-plane-plan.md`，
> 引用「§X」= 本篇对应小节（另署名的除外）。Python 原实现一律指 `reverse-proxy/admin/*.py`，
> Rust 实现对指 `crates/wb-switch-proxy/src/*.rs`。

---

## 0. 一句话目标

**账号池上能被用户点到的每一个按钮，都必须真的落到上游或者给出可读的失败原因；界面上显示的每一个字段，都必须有服务端给的值——不许再出现「按钮点了报 501」和「整列空着」两种形态。**

三处用户已实际遇到的缺口（本轮输入）：

| # | 现象（用户原话） | 结论 |
| --- | --- | --- |
| 1 | 批量签到 / 批量旅行 / 批量活跃上报未实现，提示「批量动作 travel 尚未迁移」 | `admin.rs::pool_action` 只放行 `status`，其余一律 501 |
| 2 | 单操作也为迁移，提示「动作 travel-claim 尚未迁移」 | `admin.rs::account_action` 只认 `status`/`checkin`/`refresh` |
| 3 | 凭据有效期为未显示 | `pool.rs::rows()` 不产出 `expires_at` / `expired` / `refresh_available`，前端整段静默不渲染 |

另有一条不属于业务逻辑、但同样挡路的错误，见 §6（`format v4 tool/result at seq 29 ...`）：它发生在**会话日志层**，不是本项目代码，本轮只做定性 + 上报材料，不写修复代码。

---

## 1. 现状盘点（对照代码，2026-10-31）

### 1.1 「动作」这一层的实际状态

| 入口 | 前端调用 | Rust 现状 | Python 原实现 |
| --- | --- | --- | --- |
| `POST /admin/api/accounts/{aid}/actions/{action}` | `rp.accountAction(id, action)`（`src/lib/reverse-proxy.ts:697`） | `admin.rs:628-638`：`status`/`checkin`/`refresh` 三个分支，其余 → **501「动作 {other} 尚未迁移」** | `server.py:1223-1229` → `pool.operate(aid, action)` → `pool.py:1489 _operate` 全量支持 |
| `POST /admin/api/pool/actions/{action}` | `rp.poolAction(action)`（`src/lib/reverse-proxy.ts:705`） | `admin.rs:655-661`：非 `status` 一律 **501「批量动作 {action} 尚未迁移」**；`status` 自己用串行 for 循环，**不开并发** | `server.py:1231-1236` → `pool.batch(action)` → `pool.py:1563`，`Semaphore(3)` + `asyncio.gather` |

前端按钮与动作的对应（`src/components/reverse-proxy/accounts-panel.tsx`）：

| 按钮 / 位置 | 行号 | 动作 | 当前结果 |
| --- | --- | --- | --- |
| 顶部「查询全部积分」 | `354` | `poolAction("status")` | ✅ 可用 |
| 顶部「批量签到」 | `357` | `poolAction("checkin")` | ❌ 501 |
| 顶部「批量派旅行」 | `358-372` | `poolAction("travel")` | ❌ 501 |
| 顶部「批量活跃上报」 | `373-388` | `poolAction("active")` | ❌ 501 |
| 行内「查询积分」 | 行内 | `accountAction(id,"status")` | ✅ |
| 行内「签到」 | 行内 | `accountAction(id,"checkin")` | ⚠️ 可用但与 Python 语义不同（见 §3.2：**不查状态直接提交**） |
| 行内「派旅行」 | 行内 | `accountAction(id,"travel")` | ❌ 501 |
| 行内「领奖」 | 行内 | `accountAction(id,"travel-claim")` | ❌ 501（用户截图报错的就是这条） |
| 行内「活跃」 | 行内 | `accountAction(id,"active")` | ❌ 501 |
| 行内「刷新凭据」 | 行内 | `accountAction(id,"refresh")` | ⚠️ 可用但**不回写状态**，见本文 §3.2 |

> 另外两处「有按钮但没动作」的，一并记在这里免得实现时漏：`移出池`（`deleteAccount`）与`限速`（`editAccount`）走的是
> PATCH / DELETE，不属于本轮 action 集合，现状可用。

### 1.2 账号池字段的实际形状

`pool.rs::rows()`（`:314-372`）产出的字段与前端 `RpAccount`（`src/lib/reverse-proxy.ts:149-230`）对差。
Rust 现在只给这 19 个键（逐字核对过 `pool.rs:347-369`）：

```
id, uid, name, variant, domain, enabled, pool_state, needs_relogin, auth_invalid, has_token,
cooldown_until, remaining, credits_updated, credits_stale, last_error,
request_count, consecutive_fails, inflight, model_cooldown
```

对照 Python `rows()`（`pool.py:529-601` + `switch_store.account_rows()` `:777-826`）：
（下表只列**有差异**的字段；其中 `model_cooldown` 是 Rust 独有的上游冷却形态，前端不读，本次不处理。）

| 前端读的字段 | Rust `rows()` | Python `rows()` | 界面表现 |
| --- | --- | --- | --- |
| `id/uid/name/enabled/pool_state` | ✅ | ✅ | 正常 |
| `expires_at` / `expired` / `refresh_available` | ❌ **整组缺失** | ✅（`switch_store:811-817`，读账号库 `expiresAt` / `refresh_token`） | **「凭据有效期」整列空白**（= 本轮问题 3） |
| `realm` / `realm_label` / `checkin_supported` | ⚠️ 只有 `variant` | ✅ 三个都给 | 前端回落到「国内版」badge，国际版判据丢失 |
| `nickname` / `needs_relogin_reason` | ⚠️ nickname 合进了 `name` | ✅ 分开 | 卡片副标题取 `a.uid` 或 `a.nickname`，现在永远走 uid |
| `today_checked_in` | ❌ | ✅（`checkin_date == today`） | 行内永远「今日未确认签到」 |
| `travel_label` / `travel_same_day` / `travel_location` / `travel_arrive_at` / `travel_reward` / `travel_skip` | ❌ | ✅（`pool.py:548-553`） | `travelNote()` 恒返回 null |
| `active_date` / `active_today` / `active_streak` / `active_error` / `active_skip` | ❌ | ✅（`pool.py:557-567`） | `activeNote()` 恒 null |
| `capacity` / `used` / `packages` | ❌（`billing::credits` 有 `packages`，但没进 status 落盘） | ✅ | 对账脚本读不到容量 |
| `weight` / `max_concurrency` / `min_interval` / `inflight` | ⚠️ 只有 `inflight` | ✅ | 限速弹窗看不到当前值 |
| `active`（当前账号） | ❌ | ✅（`switch_store:798`，`aid == data["active"]`） | 「手动 / 测试」badge 永不出现 |

> **结论**：`rows()` 不是「少一个字段」，而是**少一整层「账号库投影 + 运行态投影」**。
> 本轮必须把它按 Python 的字段表逐条补齐（§5），否则旅行/活跃就算跑通了，界面上也看不见结果——
> 那会变成「后台做了，用户以为没做」，比 501 更难排查。

### 1.3 任务流水（`checkin_logs`）的实际状态

| 位置 | Rust | Python |
| --- | --- | --- |
| 读 | `admin.rs:700` 原样返回 state.json 的 `checkin_logs`（**新的在后**） | `pool.task_logs()` 反转成**新的在前**，并支持 `limit` / `kind` |
| 摘要 | `admin.rs:712` 只有 `{total, failed, ok}` | `pool.task_summary()` 按类别 × 时间窗给 `success_rate` |
| 清空 | `admin.rs:734` 全清，**不支持 `kind`** | `pool.clear_task_logs(kind)` |

界面 `loadTaskLogs(kind)`（`accounts-panel.tsx:248-254`）把 `kind` 作为**查询参数**发过去，Rust 端点不接参数——所以「全部类别 / 签到 / 派猫猫旅行 / 凭据保活 / 活跃上报」这个筛选项在 Rust 引擎下**点了没反应**。
这是「动作补齐后必然暴露」的下一个缺口，与 §4 同批做。

### 1.4 已有的、可以直接复用的 Rust 资产

**这是本轮能做得快的根本原因**：旅行与签到协议在 `wb-switch-core` 里已经有两版实现。

| 需要的能力 | 已有实现 | 位置 | 能直接用吗 |
| --- | --- | --- | --- |
| 旅行状态机（depart / status / claim / 错误归类 / 缓存合并） | `travel.rs` | `crates/wb-switch-core/src/modules/travel.rs`（1322 行） | **能，但只对桌面端账号库**：它用 `load_accounts()` / `account::find_account()` / `save_travel_cache()`，全是写死 `~/.wb-switch-plus/` 的全局路径 |
| 单账号派发 | `depart_travel_for_account(&account)` | `travel.rs:384` | 同上 |
| 单账号对账 + 领奖 | `claim_travel_for_account(&account, prior)` | `travel.rs:689`（私有） | 同上 |
| 旅行缓存读写 + 并发锁 | `load_travel_cache` / `save_travel_cache` / `with_travel_cache_lock` | `config.rs:663/676/36` | 同上（`store_dir()` 写死） |
| 签到（查状态 → 判断 → 提交 → 日志） | `checkin.rs`：`get_checkin_status` / `perform_checkin` / `checkin_account` | `crates/wb-switch-core/src/modules/checkin.rs:208/256/307` | 同上 |
| 鉴权头 | `build_auth_headers(account)` | `account.rs:536` | ✅ 纯函数，`&Value` 进 `HashMap` 出，可复用 |
| 令牌刷新 | `refresh_account_token(account)` / `ensure_fresh_token(account, cfg)` | `refresh.rs:41/149` | ⚠️ 返回**新的 account 对象**，不落盘；反代要的是「换掉内存里的 token」，语义能对上 |
| 档位能力声明 | `WbVariant::supports_travel()` / `supports_checkin()` / `api_endpoint()` / `billing_paths()` | `variant.rs:310/320/161/194` | ✅ 纯函数 |
| 通用 HTTP | `http_request(url, method, body, headers)` | `config.rs:1036` | ✅，且已带 UA 与 30s 超时 |

**唯一的硬障碍**：`travel.rs` 的入口都是「读写桌面端账号库 + 桌面端缓存文件」的全局函数，没有注入点
（`depart_travel_for_account` 内部走 `ensure_fresh_token` → `load_checkin_config()` → `load_accounts()` 全链全局）。
反代的账号库路径来自 `ProxyConfig.accounts_path`（`config.rs:44`），缓存落点又是 `state.json`——
和桌面端的 `travel_cache.json` **不是同一份**。§3.1 就是解决这个。

---

## 2. 目标形态

```
React 面板（accounts-panel.tsx，零改动）
  → rp.accountAction / rp.poolAction
    → 进程内通道 → axum Router（admin.rs）
      → account_action / pool_action（本轮重写这两个 handler，删掉 501 分支）
        → 任务执行层（新增 src/tasks.rs：单账号串行锁、批量并发、流水、状态记账）
          ├─ wb-switch-core::travel（传 TaskStore：反代实现读写 state.json.travel_states）
          ├─ wb-switch-core::checkin（查状态 / 提交 / 幂等）
          ├─ src/report.rs（新增：活跃上报 /v2/report + 连登回读 /activity/growth/streak）
          └─ src/billing.rs（已有：积分 / 签到 / 刷新；保留给 status 与 refresh）
```

三条硬约束：

1. **协议只有一份**：旅行与签到的「怎么发、怎么判、失败怎么归类」必须落在 `wb-switch-core`，反代只做适配。
   任何在 `crates/wb-switch-proxy/` 里重新实现一遍 `/activity/growth/buddy/travel/*` 的写法都属于回归——
   桌面端与反代对同一个账号给出不同结论，是历史遗留问题（`reverse-proxy/admin/travel.py:1-17` 的注释就是在讲这件事）。
   例外只有一个：**活跃上报**（`report.py` 那套）桌面端没有，放 core 会造出只有反代在用的模块，所以它落在 `wb-switch-proxy`。
2. **落盘边界不变**：账号库只读（`WB_SWITCH_WRITE_BACK` 那条口子沿用 v2 的口径，本轮不动）；
   反代自己的运行态只写 `state.json`；**旅行缓存写反代的 state，不写桌面端的 `travel_cache.json`**（理由见 §3.4）。
3. **不许再有静默 501**：本轮结束时 `admin.rs` 里 `account_action` / `pool_action` 的 `NOT_IMPLEMENTED` 分支必须消失；
   做不到的能力（例如批量扫码）按 v2 §3.5 的既有原则**隐藏按钮**，而不是留一个点了报错的入口。

---

## 3. 设计

### 3.1 先解决注入问题：`travel.rs` / `checkin.rs` 加「账号来源」参数，而不是复制一份

三个候选，权衡后选 **(a)**：

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| (a) **显式传参**（选它） | 新增 `travel::depart_for(store, aid)` / `claim_for(store, aid)` 这类入口，`load_accounts()` / `load_travel_cache()` 换成 `store.<fn>()`；旧入口改成一行转调「默认 `DesktopStore`」 | `travel.rs` 内约 12 处全局调用要改；桌面端调用点零改动（旧函数签名不变） |
| (b) `#[cfg]` / 全局可变单例改指向 | 进程启动时把「账号库路径 + 缓存路径」设为全局 | 与「反代与桌面端同进程」冲突：同一进程里两个宿主会抢同一个全局 |
| (c) 反代侧另写一份 | 把 `travel.py` 的策略照抄到 Rust | 协议三处漂移（桌面端 / 反代 / Python），正是本轮要消灭的形态 |

**(a) 的具体形状**（示意，不是最终代码）：

```rust
// wb-switch-core/src/modules/travel.rs 内新增
pub trait TaskStore {
    /// 该账号当前的记录（已按 id/uid 匹配好），没有则 None。
    fn account(&self, account_id: &str) -> Option<Value>;
    /// 读/写旅行缓存。反代实现成 state.json 的 travel_states，桌面端实现成 travel_cache.json。
    fn travel_entry(&self, account_id: &str) -> Value;
    fn save_travel_entry(&self, account_id: &str, entry: Value) -> std::io::Result<()>;
    /// 令牌刷新后的回写（默认空实现 = 只留内存）。
    fn store_refreshed(&self, _account_id: &str, _refreshed: &Value) {}
}

pub async fn depart_for(store: &dyn TaskStore, account_id: &str) -> Value;
pub async fn claim_for(store: &dyn TaskStore, account_id: &str) -> Value;
```

桌面端现有入口（`depart_travel_for_account` / `run_travel_cycle`）保留签名，内部用 `DesktopStore` 转调——
**回归面为零**，`travel.rs` 里 1200 行逻辑一行不重写。

> 为什么不索性让反代也读桌面端的账号库路径：`ProxyConfig.accounts_path` 在当前安装下**恰好**等于
> `~/.wb-switch-plus/accounts.json`（`config.rs:44` 直接取 `config::accounts_file()`），但它是可覆盖的
> （`WB_SWITCH_HOME` 会同时改变两边的家目录；测试与自建部署另有路径），而**旅行缓存**的落点两边本来就不同
> （桌面端 `travel_cache.json` vs 反代 `state.json.travel_states`）。把「数据放哪」交给 trait，比赌两个路径永远相等安全。

### 3.2 动作语义逐一冻结（对齐 Python，注明差异）

| 动作 | 语义 | 上游调用 | 落盘 | 备注 |
| --- | --- | --- | --- | --- |
| `status` | 查积分并更新 `credits_updated` | `POST /v2/billing/meter/get-user-resource` | `account_status[a].remaining/packages/credits_updated/auth_invalid=false` | 保留现有 `billing::credits`，**补写落盘**（现在只回给前端不落盘，所以「已知总积分」刷新后还在靠上一轮） |
| `checkin` | 查状态 → 未签则提交 → 回读确认 | `checkin-activity-status` → `daily-checkin` | `checkin_date=today`、流水一条 `kind=checkin` | **改用 core 的 `checkin_account`**：现在的 `billing::checkin` 不看状态直接提交，国际版会被判失败；core 版先判 `supports_checkin()` 再短路 |
| `refresh` | 强制刷一次凭据 | `POST /v2/plugin/auth/token/refresh` | `token_refreshed`、`auth_invalid=false`、`cooldown_until=0` | 现状只返回不落盘（`billing.rs:146` `"persisted": false`）→ 界面「刷新凭据」点了没有反馈。本轮落盘到 `state.json`（**不写账号库**，与 v2 口径一致） |
| `travel` | 读配置 → 依次试地点 → 派发 → 回读状态 | `GET .../config`、`POST .../depart`、`GET .../status` | `travel_states[a]`、流水 `kind=travel` | 走 `travel.rs` 的状态机；国际版直接短路「国际版账号没有成长中心，已跳过」 |
| `travel-claim` | 读状态 → `arrived` 领奖 / `traveling` 等待 / `idle`+上限记当日结束 | `GET .../status`、`POST .../claim` | 同上 | 用户截图报错的就是这条 |
| `active` | 逐条发 `chat_request_send` 事件（**body 是数组**，条间隔 1.5 s）→ 回读连登天数自检 | `POST /v2/report`（billing 域，**不带** web 三头）→ `GET /activity/growth/streak` | `active_date` / `active_streak` / `active_error`、流水 `kind=active` | 口径见下方「活跃上报协议」 |

**活跃上报协议**（照 `reverse-proxy/admin/report.py` 逐条搬，别自己发挥——每一条都对应一个「错了就静默失效」的坑）：

| 约束 | 值 | 错了会怎样 |
| --- | --- | --- |
| 端点 | `POST {BASE}/v2/report`，`BASE = https://www.codebuddy.cn` | 打错域（例如成长中心域）必然失败 |
| 请求头 | **复用账号池鉴权头，不加 `Origin` / `Referer` / `x-client-platform`** | 与旅行是**两个域**：travel 走 growth 带 web 三头，report 走 billing（签到那套头） |
| body | **数组**，一条上报 = 数组里一个事件（不是一次塞 N 条） | 发对象直接 400 |
| `userId` | **必填**，取自 `X-User-Id`（必须与请求头同值） | 缺了照回 200，但事件被**静默丢弃** |
| 事件字段 | 完整形状（`eventCode/timestamp/conversationId/requestId/.../agentName/agentType`），见 `chat_event()` | 精简版会被上游后续加严挡掉 |
| `conversationId` | 本轮共用 `wb-switch-<ms>`；`requestId` 各自 `wb-switch-<ms>-r<i>` | 模拟不出「同一会话 N 轮」 |
| 条数 | `clamp_count` 落在 **[1,5]**，默认 1（`active_reports` 可调，5 = 补满领养猫的 5 次对话门槛） | 超出区间上游口径不认 |
| 节流 | 两条之间 `sleep(1.5s)`，**在下一条请求之前** | 同号秒发 N 条 = 风控信号 |
| 失败处理 | 某条失败就停在本号，不续发 | 续发既没必要（按天幂等）又多打几次 |
| 生效判据 | 回读 `GET /activity/growth/streak` 的 `data.streak.days`；`days > 0` 才算生效 | 「200 但静默丢弃」——**HTTP 200 ≠ 生效** |
| 探针容错 | 回读失败 → `ok=false`（探不了 ≠ 没生效），**不写** `active_streak` | 把读失败判成没生效，会白打一整轮 |
| 国际版 | 终局跳过（`skip=intl`，按天记 `active_skip_date`） | 走另一计费域，发国内主机会话必然失败，且失败重试一万次也不会好 |

Rust 侧对应实现位置：`crates/wb-switch-proxy/src/report.rs`（新增），函数签名与 `travel.rs` 同一风格
（`report(headers, count) -> Value` / `streak(headers) -> Value`），由 `tasks.rs` 的 `active` 分支调用。
**不要**接进 `wb-switch-core`：桌面端没有活跃上报这个功能（`travel.rs` / `checkin.rs` 里也没有对应协议），
放 core 会造出一个只有反代在用的模块。

### 3.3 批量：并发形状与「一把锁」

Python 的形状（`pool.py:1563-1576`）：

```python
if self.jobs.locked(): raise HTTPException(409, "已有批量任务进行中")
async with self.jobs:
    ids = [启用中的账号]
    semaphore = asyncio.Semaphore(3)
    return {"results": await asyncio.gather(*(run(aid) for aid in ids))}
```

Rust 对应：

| Python | Rust |
| --- | --- |
| `self.jobs`（`asyncio.Lock`，非阻塞 `locked()`） | `tokio::sync::Semaphore::new(1)` + `try_acquire()`；拿不到 → `409 已有批量任务进行中，请稍后刷新` |
| `Semaphore(3)` 并发 | `tokio::sync::Semaphore::new(3)`，`Arc` 传进 `join_all` |
| `asyncio.gather` | `futures_util::future::join_all`（`futures-util` 已在 `Cargo.toml`） |
| `asyncio.to_thread(pool.operate, ...)` | 直接 `await`（Rust 侧本来就在 tokio 里，别再 `spawn_blocking`） |

两个必须显式处理的点：

1. **单账号串行化**。Python 用 `operation_lock(aid)`（`pool.py:400`）保证同一个账号同时只有一个动作在跑。
   Rust 侧没有这把锁：批量派旅行与用户手点行内「派旅行」可能同时打到同一个账号，**上游会返回 `already traveling`**，
   于是两边各记一条结论，界面上出现「今日已派」和「派发失败」两条流水。
   做法：`Pool` 里加 `per_account: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>`，
   动作入口先 `try_lock`，拿不到就返回「该账号正在执行其它任务，请稍后」——**不排队**（排队会让批量任务卡在一个人工点击后面）。
2. **批量里单个账号失败不能掀翻整批**。Python 用 `{"ok": false, "message": ...}` 兜住；Rust 现在 `?_` 直接冒泡成 502。
   改成：每个账号的 `Result` 折成 `{id, ok, message}`，**HTTP 永远 200**，失败信息进 `results[]`
   （前端 `runBatch` 就是把 `results[].message` 拼起来显示的，`accounts-panel.tsx:283`）。

### 3.4 旅行缓存写哪：**写反代的 state，不写桌面端的 `travel_cache.json`**

| 候选 | 后果 | 结论 |
| --- | --- | --- |
| 写 `~/.wb-switch-plus/state.json` 的 `travel_states`（Python 原口径） | 与 `switch_store` 的「服务只管自己」边界一致；不碰桌面端文件 | ✅ **选它** |
| 写桌面端 `travel_cache.json` | 两套缓存同源会打架：桌面端后台循环也在写，且它的 `completed` 语义是「今日全部完成」，反代只关心单个账号 | ❌ |

落盘形状直接沿用 Python `pool.set_travel_state()`（`pool.py:1257-1267`）：

```json
{ "date": "2026-10-31", "updated": 1793413382, "ok": true, "state": "idle",
  "claimed": true, "skip": "daily-limit", "message": "今日已派",
  "recordId": 123, "arriveAt": 1793387655, "locationName": "咖啡馆", "rewardCredit": 5 }
```

> 键序与字段集合必须和 Python 一致：`rows()` 会把这些字段**原样**透给前端（`travel_arrive_at` ← `arriveAt`），
> 换名字等于换契约。

### 3.5 凭据有效期：数据从哪来，为什么现在是空的

**根因**（已用真实数据核对）：

- 账号库记录里字段叫 **`expiresAt`**（毫秒）与 **`refreshExpiresAt`**，例：
  `{"uid":"713a...","expiresAt":1796009812159,"refreshExpiresAt":1798601811159}`
- Python 在 `switch_store.account_rows()`（`:811-817`）把它投影成 `expires_at` / `expired` / `refresh_available`：
  ```python
  expiry = rec.get("expiresAt", 0) or 0
  "expires_at": expiry,
  "expired": bool(expiry) and expiry < time.time() * 1000,
  "refresh_available": bool(rec.get("refresh_token")),
  ```
- Rust 的 `AccountView`（`crates/wb-switch-proxy/src/accounts.rs:11-26`）**根本没有这两个字段**，
  `rows()` 也就不可能给出来；前端 `accounts-panel.tsx:518-519` 读 `a.expires_at` 拿到 `undefined` →
  `fmtTime(null)` → 渲染成空串，于是整列看起来「没显示」（不是 `—`，是**空白**）。

**改法**（三处，都很小）：

1. `AccountView` 加三个字段：`expires_at_ms: i64`（0 = 未知）、`has_refresh: bool`、`needs_relogin_reason: String`；
   `from_value()` 里读 `expiresAt` / `refresh_token`，与 Python 同口径（`bool(refresh_token)`，注意**加密信封也算有**）。
2. `rows()` 输出 `expires_at`（**毫秒，原样透传**，前端 `fmtTime(a.expires_at / 1000)` 自己除）、
   `expired`（`expiry > 0 && expiry < now_ms`）、`refresh_available`（**就是 `has_refresh`，不加别的判断**）。
3. 前端**不改**：`accounts-panel.tsx:518` 已经写成 `fmtTime(a.expires_at ? a.expires_at / 1000 : null)`，
   拿到 `undefined` 时整段静默——补上字段即可。`—` 与空白要能区分：`expires_at` 为 0 时前端会显示 `—`，
   所以**服务端必须给 0 而不是 null**，别让「读不出来」和「没有过期时间」长得一样。

> 顺带修掉的同类问题：`realm` / `realm_label` / `checkin_supported` / `today_checked_in` / `travel_*` / `active_*` / `capacity` / `used` / `packages` / `weight` / `active` 一并按 §5 补齐。
> 这些一次性补齐比逐个报修划算——它们全部来自同一个「`rows()` 没做投影」的原因。

### 3.6 任务流水补齐（与 §4 同批，否则补完动作看不到结果）

| 端点 | 现状 | 目标 |
| --- | --- | --- |
| `GET /pool/checkin-logs?limit&kind` | 原样返回 `state.json.checkin_logs`，旧的在前，不认 `limit` / `kind`（`admin.rs:700-710`） | 反转（新的在前）、`kind` 过滤、`limit` 截断（默认 200，上限 500） |
| `GET /pool/checkin-logs/summary?hours` | 只给 `{total, failed, ok}`（`admin.rs:712-732`） | 按 `TASK_KINDS`（checkin/travel/keepalive/active）× 时间窗给 `{kind,total,ok,failed,last,success_rate}` |
| `POST /pool/checkin-logs/clear?kind` | 全清，不认 `kind`（`admin.rs:734-750`） | 给了 `kind` 只清那一类，返回清掉的条数 |

流水条目形状（`pool.log_task()` `:417-440`，**逐字对齐**）：

```json
{"time": 1793413382, "id": "713a...", "name": "喵喵喵🌸", "kind": "travel",
 "ok": true, "message": "今日已派", "source": "manual", "credits": 5}
```

`source` 取值：`manual`（用户点）/ `batch`（批量动作）。**必须区分**——否则「我明明没点，为什么今天派了旅行」无法回答。

---

## 4. 分阶段实施（本节为唯一权威切分）

> 每个阶段独立可编译、可测试、可回滚。P-A0 是前置：**确认不了活跃上报协议，就先把它降级掉**。

### P-A0 事实核对（0.5 天，不改代码）

- [ ] 抓一次真实 Python 侧请求，确认旅行请求头集合与 body：`reverse-proxy/data/requests.log`（31 MB）里
      grep `/activity/growth/buddy/travel`；同时核对 `report.py` 的 `/v2/report` 与本计划 §3.2 表逐条一致。
- [ ] 核对 §5 字段表与 `switch_store.account_rows()` / `pool.rows()` 逐条一致（含缺失字段的默认值）。
- [ ] 确认 `active_reports` 在 `state.json.pool` 里的当前值（现状默认 1）与界面 `active_reports` 的联动。

### P-A1 `wb-switch-core` 注入化（1.5 天）

- [ ] `travel.rs`：抽出 `TaskStore` trait（`account` / `travel_entry` / `save_travel_entry` / `store_refreshed`），
      新增 `depart_for(store, aid)` / `claim_for(store, aid)`；原 `depart_travel_for_account` 改为转调 `DesktopStore`。
- [ ] `travel.rs::claim_travel_for_account` 由私有转公开（`sync_for`），保持 `merge_claim_state` 语义不变。
- [ ] `checkin.rs`：`checkin_account` / `perform_checkin` / `get_checkin_status` 已经接受 `&Value` 账号，**不动**；
      只需确认它们内部不读全局账号库（核对：只有 `load_checkin_config()` 读全局配置，签到协议本身不读）。
- [ ] **回归判据**：`cargo test -p wb-switch-core` 现有旅行/签到用例**零改动全绿**；桌面端 `travel.rs` 的行为单测（`run_travel_cycle` 等）同样零改动。

### P-A2 反代侧任务执行层（2 天）

- [ ] 新增 `crates/wb-switch-proxy/src/tasks.rs`：
  - `TaskStore` 的反代实现（账号来自 `ProxyConfig.accounts_path`，缓存来自 `state.json.travel_states`）；
  - `operate(aid, action, source)`：单账号串行锁 + 动作分派 + 结果折成 `{id, ok, message, ...}` + 落流水；
  - `batch(action)`：`Semaphore(3)` + 一把 `jobs` 锁 + 409。
- [ ] 新增 `crates/wb-switch-proxy/src/report.rs`：照 §3.2 的协议表实现 `report()` / `streak()` / `clamp_count()`
      （`clamp_count` 与 `pool.py` 同口径 `[1,5]`，默认 1）。
- [ ] 新增 `crates/wb-switch-proxy/src/http_client.rs`（或并入 `billing.rs`）：
  旅行要带 `x-client-platform: web` / `Origin` / `Referer`（`travel.py:57-68` 与 `travel.rs:78-87` 同一约束），
  **活跃上报不带这三个头**；两者都要能带上该账号的出口线路（`egress`）。
  **现在 `billing.rs` 硬编码 `no_proxy()`**，`egress()` 在 Rust 侧也只读不生效——
  P-A2 只做「能拿到出口就带上」，不追平 Python 的全量 egress 语义（见 §7）。
- [ ] `admin.rs`：`account_action` / `pool_action` 改为转调 `tasks::`，删掉两处 `NOT_IMPLEMENTED` 分支。

### P-A3 账号池投影补齐（1 天）

- [ ] `accounts.rs::AccountView` 加 `expires_at_ms` / `has_refresh` / `nickname`（独立字段）/ `needs_relogin_reason`。
- [ ] `pool.rs::rows()` 按 §5 表逐条补齐；`active` 从 `state.json.active` 读（手动模式当前账号，Python 口径）。
- `travel_label` 按 Python `travel_label()`（`pool.py:603-618`）四档实现：`traveling` / `no-buddy` / `finished` / `untraveled`。
- `pool_state` 的 `invalid` 判据含 `needs_relogin || status.auth_invalid`（Python `state()` 同口径；
  Rust `pool.rs:329` 已对齐，**补字段时不要把它拆散**）。
- [ ] 任务流水三端点按 §3.6 补齐。

### P-A4 界面收口（0.5 天）

- [ ] `accounts-panel.tsx`：**不需要改**——所有字段名都已存在（`reverse-proxy.ts:149-230`）。唯一例外：
      批量结果的 `batchResult` 现在把整批几十个账号的 message 拼成一行（`:283`），
      账号多了会糊成一片 → 改成「N 成功 / M 失败」，失败项单独列（超出 3 条折叠）。
- [ ] 按 §7 隐藏未实现能力（本轮应无：六个动作与批量全部落地；若 P-A2 的 `active` 因上游变更卡住，则隐藏「批量活跃上报」与行内「活跃」）。

### P-A5 文档与验收（0.5 天）

- [ ] `docs/reverse-proxy-contract.md` 的 accounts / pool 两组补「副作用」与参数说明（`kind` / `limit` / `source`）。
- [ ] 本文 §8 验收清单逐条勾选；`docs/reverse-proxy-internal-plane-plan.md` §7 追加一行指向本文。

**合计：约 6 人天**（P-A0 0.5 + P-A1 1.5 + P-A2 2 + P-A3 1 + P-A4 0.5 + P-A5 0.5）。
P-A1 与 P-A3 无依赖，可并行；P-A2 依赖 P-A1；P-A4 依赖 P-A2+A3。

---

## 5. `rows()` 字段对齐表（实现时的逐条清单）

「来源」列：**库** = 账号库记录字段；**态** = `state.json.account_status[a]`；**旅** = `state.json.travel_states[a]`；
**池** = `state.json.pool` / `active` / `account_overrides[a]`；**算** = 运行时算出来的。

| 字段 | 类型 | 来源 | Python 口径 | 缺省 |
| --- | --- | --- | --- | --- |
| `id` | string | 库 `id`（缺则 `uid`） | `switch_store` 的 aid | — |
| `uid` | string | 库 `uid` | 同 | `""` |
| `name` | string | 库 `nickname`（`account_overrides[a].name` 优先） | `row["name"]` | aid |
| `nickname` | string | 库 `nickname` | 同 | `""` |
| `enabled` | bool | 池 `account_overrides[a].enabled != false` | 同 | `true` |
| `active` | bool | 池 `active == aid` | 同 | `false` |
| `realm` | string | 库 `realm` / `domain` / `variant` 归一（`cn`/`global`） | `realm_mod.realm_of` | `cn` |
| `realm_label` | string | **算**：`cn→国内版`、`global→国际版` | 同 | `国内版` |
| `checkin_supported` | bool | **算**：`WbVariant::supports_checkin()` | `realm_mod.supports_checkin` | `true` |
| `pool_state` | enum | **算**：paused → invalid → cooling → exhausted → available | `pool.state()` | `available` |
| `needs_relogin` | bool | 库 `needs_relogin` | 同 | `false` |
| `needs_relogin_reason` | string | 库 `needs_relogin_reason` | 同 | `""` |
| `auth_invalid` | bool | 态 `auth_invalid` | 同 | `false` |
| `has_token` | bool | 库 `access_token` 非空 | 同 | `false` |
| **`expires_at`** | **number（毫秒）** | **库 `expiresAt`** | **`rec.get("expiresAt", 0)`** | `0` |
| **`expired`** | **bool** | **算**：`expires_at > 0 && < now_ms` | **同** | `false` |
| **`refresh_available`** | **bool** | **库 `refresh_token` 非空** | **`bool(rec.get("refresh_token"))`** | `false` |
| `status` | string | 库存在→`ready`；不存在→`missing` | `switch_store` 同 | `ready` |
| `remaining` | number/null | 态 `remaining` | 同 | `null` |
| `credits_updated` | number（秒） | 态 `credits_updated` | 同 | `0` |
| `credits_stale` | bool | **算**：`credits_updated + 600 <= now` | 同 | `true` |
| `today_checked_in` | bool | **算**：态 `checkin_date == 今天(CN)` | 同 | `false` |
| `cooldown_until` | number（秒） | 态 `cooldown_until` | 同 | `0` |
| `last_error` | string/null | 态 `last_error` | 同 | `null` |
| `token_refreshed` | number（秒） | 态 `token_refreshed` | 同 | `null` |
| `request_count` | number | 态 `request_count` | 同 | `0` |
| `consecutive_fails` | number | 态 `consecutive_fails` | 同 | `0` |
| `last_fail_at` | number | 态 `last_fail_at` | 同 | `null` |
| `capacity` / `used` / `packages` | number/null | 态同名 | 同 | `null` |
| `travel_label` | enum | **算**：旅 + 今天 | `pool.travel_label()` | `untraveled` |
| `travel_same_day` | bool | **算**：旅 `date == 今天` | 同 | `false` |
| `travel_location` | string/null | 旅 `locationName` | 同 | `null` |
| `travel_arrive_at` | number/null | 旅 `arriveAt` | 同 | `null` |
| `travel_reward` | number/null | 旅 `rewardCredit` | 同 | `null` |
| `travel_skip` | string/null | 旅 `skip` | 同 | `null` |
| `active_date` | string/null | 态 `active_date` | 同 | `null` |
| `active_today` | bool | **算**：`active_date == 今天` | 同 | `false` |
| `active_streak` | number/null | 态 `active_streak` | 同 | `null` |
| `active_error` | string/null | 态 `active_error` | 同 | `null` |
| `active_skip` | string/null | **算**：态 `active_skip`，且 `active_skip_date == 今天` | 同 | `null` |
| `weight` / `max_concurrency` / `min_interval` | number | 态（缺省 → 池 `default_*`） | `throttle.effective_weight` / `limits_of` | `1 / 0 / 0` |
| `inflight` | number | 池运行时 | `throttle.inflight` | `0` |

> 「今天」一律按 `Asia/Shanghai`（`chrono::FixedOffset::east_opt(8*3600)`），**不要用 `Local`**：
> Python 侧是 `CN` 常量固定 +8，用本地时区会在非 +8 机器上把「今日已签到」判错一天。

---

## 6. 附带问题：`format v4 tool/result at seq 29 requires toolCallId matching its tool source`

### 6.1 它是什么

这条错误**不是本仓库的代码**。措辞（`format v4` / `tool/result` / `seq` / `toolCallId`）来自
DeepSeek Harness 的**会话日志（v4 NDJSON）记录格式**：每一行是一个带 `seq` 的事件，
工具调用记 `tool/call`、工具结果记 `tool/result`，结果必须带着与调用**相同的 `toolCallId`**，
记录层在回放/追加时会校验这个配对；配不上就整条拒收，于是界面上表现为 `Error: format v4 tool/result at seq 29 ...`。

三个可能的成因（按可能性排序）：

| # | 成因 | 表现特征 |
| --- | --- | --- |
| 1 | **工具调用被打断，结果没落盘，之后又追加了新事件**（用户中途点「停止」/ 切换模型 / 关窗）；`seq 29` 那条 `tool/result` 找不到对应的 `tool/call` | 只在事件数很少的会话上出现（空会话 + 若干工具调用），`seq` 数值偏小 |
| 2 | **会话文件被并发写**（同一会话开了两个窗口，或应用被复制/恢复） | 失败位置随机，重开可能换成另一个 `seq` |
| 3 | **事件被外部工具编辑过**（手工改 jsonl、导出再导入） | 该 `seq` 附近的工具结果缺 `toolCallId` 字段 |

### 6.2 为什么本轮不在这里修

- 代码不在本仓库：`dsh` 实现在 `C:\Users\niexiawei\AppData\Local\Programs\DeepSeek Harness\resources\app.asar`，
  是打包态（本会话实测该文件对普通读取**不可见**，只能用 Electron 运行时自带的 asar 垫片访问），
  无法定位到具体的校验函数；
- 它是宿主问题，改本项目的任何一行都不会让它消失。

### 6.3 现在就能做的两件事

**(1) 先把坏掉的那条会话救出来（无损，可回滚）**：

```
%USERPROFILE%\.dsh\sessions\<按工作区名转义>\<session-id>\session.v4.jsonl.zstd
```

- 该文件是**多帧 zstd 流拼接**（首帧能解出 `{"type":"session",...}` 头，后续帧承载事件）。
  **注意**：本会话实测「文件大小」与「能解出的事件字节数」完全不成比例（例如 525 KB → 解出 217 B，
  即只有首帧能解出）——所以**不要用文件大小判断内容是否完整**。
  截断点之后的帧解不出来，说明写入时就是断的，这本身就是成因 1 的证据。
- **做法**：先整份复制到 `session.v4.jsonl.zstd.bak-<日期>`，再删掉解不出来的尾部帧（保留首帧 + 能解的帧）。
  修复后重开该会话；若仍报 `seq 29`，说明缺的 `tool/result` 在**中间**帧，只能整条会话作废（新开会话）。
- 判断依据是**解出来的事件里 `tool/call` 与 `tool/result` 的 `toolCallId` 集合是否相等**，不是文件大小。

**(2) 反馈材料**（发出去才能修到根上）：

> 会话日志 v4 在追加 `tool/result` 时校验 `toolCallId` 与其 `tool/call` 的配对；
> 工具调用被中断（用户停止 / 关窗 / 切换模型）后，后续写入的 `tool/result` 会找不到来源，
> 整条日志被拒收并报 `format v4 tool/result at seq N requires toolCallId matching its tool source`。
> 建议：a) 中断时补写一条 `tool/result`（`error: "interrupted"`）而不是留空；
> b) 校验失败时降级为「跳过该行并告警」，不要让整份会话不可用。

### 6.4 和本计划的边界

写清楚，免得被当成「顺手一起修」：**§6 的修复不进入 P-A1…P-A5 的任何一步**，它属于 DSH 宿主。
本轮若有人要求，只做 §6.3(1) 的数据抢救。

---

## 7. 风险、遗留与明确不做

| 风险 | 缓解 |
| --- | --- |
| `travel.rs` 注入化改动面大，可能把桌面端的自动旅行弄坏 | P-A1 的验收判据是「`wb-switch-core` 现有用例零改动全绿」；`DesktopStore` 只是把原来的全局调用原样搬过去，不重写状态机 |
| 同一账号被批量与手动同时操作 | §3.3 的 `per_account` 互斥 + 不排队；冲突时给可读文案 |
| 旅行/签到打到真实上游会消耗额度或改变账号状态 | 全部动作沿用界面已有的二次确认（`accounts-panel.tsx:362-388` 的 `confirm.ask`）；**服务端不做「默认开启」**，`auto_*` 开关一律保持默认关 |
| `active` 上报的「200 但静默丢弃」 | 必须保留 `report.py` 的回读自检（`/activity/growth/streak` 的 `days > 0`）；回读失败按「探不了」处理，**不写** `active_streak`，也不把已有天数抹成 0 |
| `egress`（出口线路）在 Rust 侧只读不生效 | 明确不做：P-A2 只带「已配置的线路」上请求，不追平 Python 的 IP 归属语义；界面维持现状（v2 遗留项） |
| 加密信封 token（`$wbEncrypted`） | 沿用现状：`plain_token()` 拿不到明文就返回「该账号 token 是加密信封」，不做解密（`workbuddy_atrest_crypto` 的移植是独立议题） |
| `wb2api` 上游冷却（`up_*` 字段） | 明确不做：整组 `up_*` 取自外部 workbuddy2api，前端已按「字段缺失即不显示」处理（`upstreamNote`），补它会引入第二份冷却数据源 |

**明确不做**（避免范围蔓延）：

- 批量扫码（`onBatchScan`）——桌面端已按 v2 §3.5 隐藏，保持隐藏；
- OAuth 扫码（`admin.rs` 的 `oauth_stub`）——同 v2 决策，桌面端走主项目 OAuth；
- Python 侧任何改动（`reverse-proxy/` 是 old 入口，只作为**口径参照**）；
- 把 `pool.rs` 的运行时状态（冷却 / 请求数 / inflight）改为落盘：它现在从 `account_status` 播种、重启后由
  数据面重新累计，改成落盘会引入第二个写者（§3.4 只把 `travel_states` 这类**任务结果**落进 state.json）。

---

## 8. 验收标准

**自动化**

- [ ] `cargo test -p wb-switch-core` 全绿，且旅行/签到用例**零改动**（证明注入化没有改变行为）。
- [ ] `cargo test -p wb-switch-proxy` 新增用例：
  - `account_action` 六个动作各一条（mock 上游），断言返回形状 `{ok, message, ...}`；
  - `pool_action` 四种动作 + 并发 409 + 单账号失败不掀翻整批；
  - `rows()` 字段表逐条断言（**特别是 `expires_at` / `expired` / `refresh_available` 三项**）；
  - 流水端点：`kind` 过滤、倒序、`limit`、按类清空。
- [ ] `src/lib/reverse-proxy.test.ts` 等前端用例零改动通过（前端契约不变）。
- [ ] `npx tsc --noEmit`、`vite build` 通过。

**手工（对着截图逐条回看）**

- [ ] 顶部四个按钮：查询全部积分 / 批量签到 / 批量派旅行 / 批量活跃上报 —— **不再出现「尚未迁移」**。
- [ ] 行内「派旅行」「领奖」——正常返回；国际版给「国际版账号没有成长中心，已跳过」。
- [ ] 「凭据有效期」列**每一行都有时间**（对齐账号库 `expiresAt`），到期账号显示「已到期 · 调用时尝试刷新」。
- [ ] 「状态 / 签到」列出现「今日已签到」与旅行/活跃附注（与桌面端账号页同一口径）。
- [ ] 任务流水筛选五档都生效，新条目在最前，`source` 能看出是手动还是批量。
- [ ] 批量任务进行中再点一次 → 「已有批量任务进行中，请稍后刷新」（不是 501、不是 500）。
- [ ] 全程 `DevTools → Network` 无 `/admin/api/*` 请求（v3 的红线，不能被本轮破坏）。

---

## 9. 与既有文档的关系

| 文档 | 关系 |
| --- | --- |
| `reverse-proxy-rust-migration-plan.md`（v1） | 其 P0–P5 把「协议层」迁完了，本文补的是**账号池任务层**（v1 §2.1 表里 `pool.py` 的 2198 行） |
| `reverse-proxy-integration-plan.md`（v2） | 本文沿用其决策 2（共用数据目录）、决策 3（宁缺毋滥，不摆废按钮） |
| `reverse-proxy-internal-plane-plan.md`（v3） | 本文不改管理面传输层，只在 `admin.rs` 的 handler 内部换实现 |
| `reverse-proxy-contract.md` | 本文 §8 验收后需同步其 accounts / pool 两组 |
---

## 10. 落地记录（P-A0 / P-A1 / P-A2，2026-10-31）

### 10.1 实际改动

| 文件 | 性质 | 内容 |
| --- | --- | --- |
| `crates/wb-switch-core/src/modules/task_store.rs` | **新增** | `TaskStore` trait + `DesktopStore`（账号库/`travel_cache.json` 全局实现，逐字沿用旧路径） |
| `crates/wb-switch-core/src/modules/travel.rs` | 改 | `depart_travel_for_account` / `claim_travel_for_account` / `sync_account_for_dispatch` 拆成「转调 `with_store` 版本」；协议体一行未改 |
| `crates/wb-switch-proxy/src/report.rs` | **新增** | `/v2/report` 逐条上报 + `/activity/growth/streak` 回读自检；36 字段事件形状 |
| `crates/wb-switch-proxy/src/tasks.rs` | **新增** | `TaskRunner`：六动作、单账号串行锁、批量 `Semaphore(3)` + jobs 409、流水写入/读取/摘要 |
| `crates/wb-switch-proxy/src/admin.rs` | 改 | `account_action` / `pool_action` 转调 `tasks`，**两处 `NOT_IMPLEMENTED` 分支已删除**；流水三端点接 `limit`/`kind`/`hours` |
| `crates/wb-switch-proxy/src/http.rs` | 改 | `AppState` 增 `tasks` 字段（批量锁与串行锁必须跨请求存活） |
| `crates/wb-switch-proxy/src/config.rs` | 改 | 新增 `with_billing_base` |
| `crates/wb-switch-proxy/Cargo.toml` | 改 | 加 `chrono`（固定 +08:00 判「今天」） |
| `crates/wb-switch-proxy/tests/tasks.rs` | **新增** | 16 例：动作识别、结果形状、国际版短路、批量 409、流水四态 |

### 10.2 与计划的偏差（都是实测发现，比计划更准）

| # | 计划里写的 | 实际做法 | 为什么 |
| --- | --- | --- | --- |
| 1 | `checkin` 改用 core 的 `checkin_account` | ✅ 照做 | 但 `status` **没用** core 的签到状态接口：它不带 `remaining`，会让积分列永远是 `—`。积分仍走 `billing::credits` |
| 2 | 未知动作「501→404」 | ✅ | 实测 `actions/`（空动作）连路由都匹配不上，走框架兜底 404（无 JSON body）。测试已按真实契约断言 |
| 3 | 批量并发 3 + 一把锁 | ✅ | 断言写成「409 或 200」，因为本地 mock 下第一批可能瞬间跑完；硬断言 409 会变成 flaky |
| 4 | `report` 放 `wb-switch-proxy` | ✅ | `report.py` 确实存在（§3.2 表格依据它），先前「找不到该模块」的判断是错的，已在正文修正 |

### 10.3 测试

`
cargo test -p wb-switch-core      767 passed（+ 5 e2e），旅行/签到用例零改动
cargo test -p wb-switch-proxy     94 lib + 8 admin + 2 chat + 6 catalog + 6 serve + 4 skeleton + 16 tasks
cargo check --workspace --all-targets   0 error（告警仅来自 vendor/）
`

**P-A1 的验收判据「core 现有用例零改动全绿」已满足**：`travel.rs` 的 1300 行状态机没有重写，
新入口只是把「数据从哪来」提到 trait 上。

### 10.4 P-A3 尚未做，已知缺口

`pool.rs::rows()` 仍缺 `expires_at` / `expired` / `refresh_available` / `realm_label` /
`today_checked_in` / `travel_*` / `active_*` / `capacity` / `weight` / `active`。
所以现阶段：

- 「凭据有效期」列**仍然是空的**（用户最初报的第 3 个问题还没修）；
- 旅行/活跃做完了，但卡片上看不到附注 —— 这正是本文 §1.2 结论「后台做了、用户以为没做」。

`tasks.rs` 已经把这个投影需要的两个纯函数准备好了：`travel_label()`（四档）
与 `checked_in_today()`（固定 +08:00）。P-A3 直接接进 `rows()` 即可。