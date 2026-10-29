# 反代服务内迁计划：Python 独立进程 → Rust / Tauri 单进程

> 状态：方案稿（未改代码）
> 目标：把 `reverse-proxy/`（FastAPI，17,166 行，独立进程 + 独立 exe）整体迁进本项目，
> 前端留在 `src/`，反代逻辑用 Rust；对外仍是 HTTP 数据面。桌面端是**唯一**反代宿主，SPA 同源直连，webui 改为通过 BFF 连本机桌面端（不再自带 Python 服务）。
> 参考现状：`reverse-proxy/`、`src/lib/reverse-proxy*.ts`、`src-tauri/src/reverse_proxy.rs`、`crates/wb-switch-server`。

---

## 1. 为什么要迁

现在的形态是「一个桌面应用 + 一个 Python 服务 + 两套管理界面」，割裂体现在六处：

| 割裂点 | 现状 | 代价 |
| --- | --- | --- |
| 进程 | Tauri 只做启动器（`src-tauri/src/reverse_proxy.rs` 找 exe / 探测端口） | 生命周期、崩溃恢复、退出清理都要跨进程协调；端口被占只能靠探测 |
| 发布 | `reverse-proxy/dist/wb-reverse-proxy.exe` 是 bundle resource（`tauri.windows.conf.json`） | 改一行 Python 就要重打包 exe；`npm run verify:release` 还要额外校验它 |
| 数据 | 桌面端在 `~/.wb-switch-plus/`，独立跑在 `reverse-proxy/data/`，exe 跑在 exe 同级 `data/` | 三套数据目录、三套 `state.json`，排障先问「你跑的是哪一个」 |
| 前端 | SPA 通过 `fetch` 打 `http://127.0.0.1:8787/admin/api/*`（`src/lib/reverse-proxy.ts`），要自己管 base URL 与 `X-Admin-Token` | 跨源、CORS、登录态与桌面端登录是两套 |
| 管理面 | Python 自带静态后台 `/admin/`，桌面端 SPA 又实现一遍 | 同一份 API 两个界面，改字段要改两处 |
| 语言 | 核心协议转换在 Python（`core/`），账号/切换/登录在 Rust（`crates/wb-switch-core`） | 同一个「账号」概念两边各有一套模型与刷新逻辑 |

迁移后的形态：

```
外部客户端（Codex / Claude Code / CLI）──HTTP──┐
                                              ├──▶ 桌面端 axum（127.0.0.1:8787）
webui（wb-switch-server，同机 BFF 转发）──HTTP──┘        │
        ▲                                              ├── wb-switch-proxy（协议 / 池 / 管理）
        │                                              └── wb-switch-core（账号库 / OAuth / 刷新）
   浏览器（只与 webui 同源）

桌面端 = 唯一反代宿主；webui 不再加载反代逻辑，只转发管理请求。
```

---

## 2. 现状盘点

### 2.1 代码规模

| 模块 | 行数 | 职责 | 迁移难度 |
| --- | --- | --- | --- |
| `admin/server.py` | 2,319 | 76 条管理路由、静态后台、中间件 | L |
| `core/converter.py` | 2,237 | OpenAI chat 转发、模型目录、鉴权、日志 | L |
| `admin/pool.py` | 2,198 | 选号、亲和、冷却、并发闸门、计费/签到/旅行 | L |
| `admin/switch_store.py` | 913 | 账号库桥接、热加载、凭据管理 | M |
| `admin/security.py` | 735 | 会话 / 口令 / 令牌 / 审计 / 防爆破 | M |
| `core/responses_projection.py` | 700 | Codex 长上下文投影 | L |
| `admin/alerts.py` | 654 | SMTP 告警 | S |
| `core/responses_adapter.py` | 555 | `/v1/responses` ↔ chat | M |
| `admin/keysvc.py` | 532 | 多密钥 / 白名单 / 额度 | M |
| `admin/travel.py` | 504 | 成长中心旅行 | M |
| `core/anthropic_adapter.py` | 491 | `/v1/messages` ↔ chat | M |
| `core/desensitize.py` | 473 | 客户端模板脱敏 | M |
| 其余 20+ 模块 | ~3,600 | 加密信封、egress、分组、限速、模型、导出、扫码登录… | S–M |
| **合计** | **17,166** | | |

### 2.2 对外契约（必须逐条冻结）

- 数据面 6 条：`/v1/chat/completions`、`/v1/responses`、`/v1/messages`、`/v1/messages/count_tokens`、`/v1/models`、`/health`（+ `/v1/usage`）。
- 管理面 76 条，按职责分组：登录/会话、密钥、账号池、分组、出口线路、用户、管理令牌、IP 安全、审计、告警、设置、用量、导出、OAuth 扫码、模型中心、CLI 接入。
- 状态文件 27 个键：`keys` / `users` / `api_tokens` / `audit_logs` / `security` / `ip_rules` / `ip_access_logs` / `account_status` / `account_overrides` / `session_bindings` / `pool` / `groups` / `egress` / `alerts` / `usage_stats` / `checkin_logs` / `travel_states` / `runtime_*` …
- 外部依赖：`fastapi`、`uvicorn`、`httpx`、`cryptography`、`smtplib`。

### 2.3 已经稳定、必须保真的行为（不能「顺手重构」）

1. 国内版请求形状：`client_profile` 透传、`extra_vars` / `cache_retention`、11128 提示词改写（`cn_prompt`）。
2. 流式语义：空闲超时、无终态帧判失败、已输出不重放（`upstream_errors` + stream guard）。
3. 池行为：会话亲和、401/403→5min、402/429→30min、`11134` 模型级冷却、并发闸门与兜底放行。
4. 模型目录：`/v3/config` + 本机产品配置 + 国内版种子，`disabledMultimodal` 优先。
5. 凭据：账号库只读、`$wbEncrypted` 信封、`WB_SWITCH_WRITE_BACK=1` 才回写。
6. 管理面安全：口令 PBKDF2、令牌只存哈希、防爆破按 IP+用户名、审计日志。

---

## 3. 目标设计

### 3.1 新 crate：`crates/wb-switch-proxy`

```
crates/wb-switch-proxy/src/
  lib.rs            # ProxyHandle: start/stop/status/port；axum Router 组装
  config.rs         # 数据目录、端口、host、默认值（复用 ~/.wb-switch-plus/reverse-proxy）
  state.rs          # state.json 读写（键名与 Python 完全一致）+ 原子落盘 + 迁移
  store/            # keys / users / tokens / audit / ip_rules / settings 子模块
  pool/             # 选号、亲和、冷却、闸门、任务（签到/旅行/保活）
  upstream/         # httpx 替代：reqwest 客户端、realm 头、client_profile、errors、stream_guard
  wire/             # chat/responses/anthropic 适配、projection、desensitize、system_identity、cn_prompt
  catalog.rs        # 模型目录（远端 + 本机 + 种子）
  billing.rs        # 额度 / 签到 / 旅行
  browser_login.rs  # 扫码登录（state/token/refresh 轮询）
  admin/            # axum 路由，镜像 76 条管理 API
  metrics.rs        # 请求日志、用量统计
```

Router 只由 Tauri 宿主启动。`wb-switch-server` **不挂载**它——webui 只做一层 BFF 转发（见 §3.4）。

### 3.2 前端分工

| 层 | 变化 |
| --- | --- |
| `src/pages/ReverseProxyPage.tsx` 与 `src/components/reverse-proxy/*` | **基本不动**（继续用 HTTP 管理面） |
| `src/lib/reverse-proxy.ts` | 只改 base URL 来源：由 Tauri 命令返回实际端口，不再让用户填 |
| `src/lib/reverse-proxy-service.ts` | 删掉「找 exe / 起进程 / 探测端口 / 提权停止」，换成 `proxy_start` / `proxy_stop` / `proxy_status` 三个 invoke |
| `src-tauri/src/reverse_proxy.rs` | 从「启动器」变成「进程内服务句柄」：启动 axum、记录端口、退出时优雅关闭 |
| 静态后台 `/admin/` | 删除；SPA 是唯一界面 |
| `crates/wb-switch-server/src/api.rs` | 新增 `/admin/api/*` BFF 转发（同机 → 桌面端），并读 `webui-token` |
| `src/pages/ReverseProxyPage.tsx` | 灰度期加「引擎」切换（Python 8787 / Rust 8788），其余页签不变 |

### 3.3 端口与监听

- 默认 `127.0.0.1:8787`，端口可配（沿用 `WB_SERVE_PORT`）。
- 外部客户端仍通过 HTTP 访问 → **不能只做 invoke**，HTTP 数据面必须保留。
- 桌面端启动时自动起监听；「局域网可访问」沿用现有 `0.0.0.0` 开关。
- 灰度期两个端口：Python 8787、Rust 8788（可配），SPA 按所选引擎切换 base URL。

### 3.4 webui 形态：BFF 转发，不自带反代

```
浏览器 ──同源──▶ wb-switch-server:/admin/api/*  ──HTTP──▶ 桌面端 127.0.0.1:8787/admin/api/*
                                                   （带上内部管理令牌）
```

**为什么不让浏览器直连 8787**：那还是跨源 + 两套登录态，等于把现在的割裂换个方向留着。BFF 后浏览器只认一个 origin。

**令牌怎么来**（同机信任）：桌面端启动反代时把一枚**短期管理令牌**写到
`~/.wb-switch-plus/reverse-proxy/webui-token`（仅当前用户可读，含过期时间与端口）；
`wb-switch-server` 启动时读取，转发时放 `X-Admin-Token`。文件缺失/过期 → webui 管理页返回
`503 桌面端未运行或未授权`，并在界面上给出「请先启动 WB-Switch-Plus 桌面端」的提示，而不是空列表。

**边界与后果**：
- webui 不再能独立提供反代管理，甚至不再能独立跑数据面——这是本决策的直接代价。
- 无头服务器（只跑 `wb-switch`、没有桌面端）将没有反代能力；如后续需要，再加「远程模式」（显式配置远端地址 + 令牌）。
- 转发只做透传：不改 body、不改状态码；审计仍由桌面端记一条（actor 标为 `webui`）。

---

## 4. 关键决策

**D1：进程内 axum，不做 sidecar。**
理由：sidecar 仍要跨进程协调生命周期与数据目录，只是把 Python 换成 Rust exe，割裂没解决。进程内可直接共享 `wb-switch-core` 的账号库与刷新逻辑。

**D2：管理面只在桌面端走 HTTP；webui 通过 BFF 连桌面端。**
理由：数据面本来就要给外部客户端用，HTTP 省不掉；但管理面不再由 `wb-switch-server` 自己实现——它只把 `/admin/api/*` 转发到本机桌面端，浏览器始终与自己同源，CORS 与登录态都不必再处理。代价是 webui 必须与桌面端同机（见风险表）。Tauri 只多三个生命周期命令。

**D3：`state.json` 键名、目录、加密方式保持兼容。**
理由：灰度期需要 Python/Rust 双向可读、可回滚；用户已有的密钥、用户、审计不能丢。新字段只增不改。

**D4：协议层 1:1 移植，先录制后重写。**
理由：`converter` / `responses_projection` / `anthropic_adapter` 是长期踩坑的产物（desensitize、identity filter、SSE 边界）。迁移期间用「同一请求 → 同一上游体 / 同一 SSE 帧序列」的 golden 测试锁住，禁止顺手优化。

**D5：池与任务先读后写。**（2026-10-29 已收尾：桌面端引擎改为可写）
第一阶段 Rust 只做选号与冷却读；签到/旅行/保活仍由 Python 跑，确认选号一致后再接管写操作。
现在 `proxy_engine` 用 `StateStore::open_writable`——池设置、密钥、账号动作都必须真正落盘，只读实例会直接 403。
为降低双写风险，`StateStore::update()` 落盘前先重读磁盘（对方刚写进去的 `account_status` / 密钥不会被整份覆盖），
但**同秒写同一处仍是后写者胜**，所以灰度期仍要求只启用一个引擎。

**D6：灰度双跑 = 双端口 + SPA 切换。**
Python 留 8787，Rust 起 8788；SPA 顶部多一个「引擎」选择器，选中哪个就把 base URL 指向哪个端口，状态存在本地。
两个引擎**共用同一份** `~/.wb-switch-plus/reverse-proxy/state.json` 与同一份 `accounts.json`（这是「一套数据、两套实现」对比的前提），
因此任一时刻只应启用一个引擎；切换器只负责改地址，不会去停另一个进程。

**D8：账号只有一份，判据只有一套（2026-10-29）。**
反代不再维护「第二份账号表」：账号、凭据、登录态、`needs_relogin` 全部以 `~/.wb-switch-plus/accounts.json` 为准，
池只在其上加一层「是否参与调度」。选号判据固定为
`paused → invalid(needs_relogin | auth_invalid | 无 token) → cooling → exhausted → available`，
Rust 与 Python 必须逐项一致；Python 的 `switch_store` 仍维护 `data["accounts"]` 视图（热加载 + 覆盖层），
但它只承载「池内改名 / 启停 / 移除」，不再是账号事实来源。
理由：出现过「账户管理显示需重新登录、反代页显示可用」——同一账号两套判据，用户无从判断该信哪个。

**D7：下掉 Python 的退出标准（不是「写完就删」）。**
契约测试全绿 + 灰度双跑 7 天无差异 + 回滚开关验证过一次，才删除 `reverse-proxy/` 与打包脚本。

---

## 5. 迁移映射（Python → Rust）

| Python | Rust 落点 | 迁移方式 |
| --- | --- | --- |
| `core/converter.py` 的 chat 路由 | `wire/chat.rs` + `admin/routes_v1.rs` | 重写，golden 对齐 |
| `core/anthropic_adapter.py` | `wire/anthropic.rs` | 重写，帧级对齐 |
| `core/responses_adapter.py` + `responses_projection.py` | `wire/responses*.rs` | 重写，最易回归，优先录制 |
| `core/desensitize.py` / `system_identity.py` / `cn_prompt.py` | `wire/normalize.rs` | 规则表逐条搬，配样例单测 |
| `core/client_profile.py` / `upstream_errors.py` | `upstream/profile.rs` / `upstream/errors.rs` | 直接搬（已有测试用例） |
| `admin/pool.py` | `pool/*` | 行为表驱动重写 |
| `admin/switch_store.py` | 直接复用 `wb-switch-core::account` + `store/accounts.rs` 薄桥 | **不重写账号逻辑** |
| `admin/keysvc.py` / `security.py` / `groups.py` / `egress.py` | `store/*` | 逐接口对齐 |
| `admin/billing`（pool 内）/ `travel.py` | `billing.rs` | 接口对齐，字段名不变 |
| `admin/browser_login.py` | `browser_login.rs` | 轮询状态机照搬 |
| `admin/alerts.py` | `alerts.rs` | `lettre` 替代 smtplib |
| `admin/settings_store.py` + `workbuddy_atrest_crypto.py` | `state.rs` | 加密格式逐字节兼容（用现有测试向量） |
| `serve.py` | `lib.rs` + Tauri 启动 | 删除 |
| `admin/static/*` | 删除 | SPA 唯一 |

---

## 6. 分阶段实施

### P0 契约冻结（✅ 已完成，2026-10-29）
- 产出：
  - `docs/reverse-proxy-contract.md`（自动生成，勿手改表）；
  - `tests/fixtures/proxy/manifest.json`：**76 条路由**（method / path / group / auth / 是否有 body / 风险等级）+ **27 个 state 键**；
  - `tests/fixtures/proxy/dataplane/`：6 个 golden（chat_cli / chat_desktop / responses / anthropic / 11128 / 不完整流）。
- 工具：`reverse-proxy/tools/record_contract.py`、`record_dataplane.py`，均离线运行，不消耗额度、不写真实状态。
- 未做（已写进契约文档「已知缺口」）：管理端点的响应体采样——多数端点会写状态或打上游，安全采样白名单留到 P5 按组补；鉴权失败路径同样留到 P5。
- **验收**：✅ 任何后续 Rust 实现都能对着 manifest + golden 自动 diff。

### P1 骨架与状态（✅ 已完成，2026-10-29）
已完成（2026-10-29）：
- 新 crate `crates/wb-switch-proxy`：`ProxyConfig` / `StateStore` / `accounts` / `http` / `ProxyHandle`。
- `StateStore`：按 27 个键补默认值、保留未知键、**只读打开**（灰度期 Python 唯一写者）；`open_writable` 供 P4 与测试用，写盘走 `wb-switch-core::config::atomic_write`。
- `accounts`：只读账号库，派生 `variant`（`variant` 字段 → domain 兜底）与 token 是否存在。
- HTTP：`/health`（账号数 / 数据目录 / 是否可写 / capabilities）、`/v1/models`（国内版种子清单）、数据面 4 条返回 501（`not_implemented`，不伪装成功）。
- Tauri：`ProxyEngine` 托管状态 + `proxy_engine_status/start/stop` 三个命令，进程内监听、优雅关闭。
- 测试：`cargo test -p wb-switch-proxy` 7 例（默认键、未知键保留、只读拒写、账号视图、health/models/501）；`cargo check -p wb-switch-rust` 通过。

后续补齐（同日）：
- 单账号 chat 透传：`/v1/chat/completions` 流式透传 + 非流式聚合；空闲 300s 超时、无终态帧补 `incomplete_stream` / `empty_response` 错误帧；上游非 2xx 走 `errors` 分类映射。
- 数据面鉴权：`state.keys` 有密钥时校验 SHA-256（Bearer / X-Api-Key），一枚都没有时放行（全新安装自测）。
- 已知限制：`$wbEncrypted` 信封 token 返回 501（解密属 P4）；`/v1/models` 仍是种子清单（真实目录在 P2）。

**验收**：✅ `cargo test -p wb-switch-proxy` 25 例全绿（含 mock 上游的流式/非流式/错误/半截流）；Tauri 编译通过。

### P2 协议保真（✅ 已完成）
已完成（2026-10-29）：
- `profile`（指纹识别 + 出站头合并）、`prompt`（身份句过滤 + 11128 改写）、`errors`（业务码 → 失败类 / 冷却维度 / 错误体）、`wire`（字段白名单 + developer→system + 默认值）。
- **golden 对齐**：`chat_cli` / `chat_desktop` 的上游请求体与 Python fixture **逐字段相同**（`wire::tests`）。
- stream guard 与聚合已用于 chat 处理器，4 个端到端用例覆盖流式、非流式、半截流、11128。
- 模型目录（`catalog.rs`）：两个远端端点合并（`/console/…/models` + `/v3/config`）+ 客户端缓存产品配置（`~/.workbuddy/cache/acc-product-config-v*.json`，取最高版本）+ 本机安装快照 + 国内版种子补缺；60s 缓存；能力字段与 Python 版同口径。
- 与 Python 的一处**有意差异**：Rust 按参考项目把 `image-to-video` 也视为非对话模型过滤掉，Python 的 `NON_CHAT_MODEL_TAGS` 漏了这一项（实测对比：Python 50 个、Rust 49 个，差的就是 `kling-v3-i2v`）。这是修 bug，不是回归；要不要同步修 Python 由 P7 退场前决定。

- 脱敏（`desensitize.rs`）：83 个词表从 Python 原样生成，词内插零宽空格，只作用于 system / developer；默认关闭，`WB_PROXY_DESENSITIZE=1` 打开（与 Python converter 默认一致，golden 也关着录）。

P2 完成。

### P3 Responses / Anthropic（✅ 已完成）
已完成（2026-10-29）：
- Anthropic 适配器（`anthropic.rs`）：`/v1/messages` 请求转换（system / messages / tools / tool_choice / stop_sequences）+ chat SSE → Anthropic 事件流（message_start / content_block_* / message_delta / message_stop），文本与 tool_use 都转；`/v1/messages/count_tokens` 粗估。
- golden：请求转换与 `anthropic_stream.json` 的上游体逐字段相同；事件序列用例覆盖正常流与半截流（报 incomplete_stream）。
- Responses 适配器（`responses.rs`）：input / instructions / tools / tool_choice / max_output_tokens 转换；chat SSE → Responses 事件（created → in_progress → output_item.added → content_part.added → output_text.delta/done → output_item.done → completed）；工具调用走 function_call_arguments 系列事件；非流式聚合成 response 对象。
- 实机对比（同一账号、非流式）：Python 8787 与 Rust 8788 的 `/v1/responses` 返回相同文本与相同 usage 数字，顶层字段集合一致（Python 多嵌套 details）。

- 长上下文投影（`projection.rs`）：逐条移植 Python 的 `responses_projection` —— harness 模板整段丢弃、保留最新用户意图与最近 8 条真实链路、更早历史压成规则摘要、tool schema 只留结构键、长参数/长工具输出压成可继续推理的摘要。**默认关闭**（与 Python `CODEBUDDY_LOSSY_PROJECTION` 默认一致），`WB_PROXY_LOSSY_PROJECTION=1` 启用。

待办：
- 真实客户端回放（Codex / Claude Code）：目前是 fixture + 单次实机调用，还没有长会话 + 工具调用的完整链路验证。

### P4 池与密钥（✅ 主体完成，3 项留到 P5/cutover）
已完成（2026-10-29）：
- `pool.rs`：round_robin / manual / least_used 三种路由；24h 会话亲和（`x-conversation-id` / `x-session-id`）；账号级与**模型级**冷却；`account_overrides` 的 hidden/enabled；积分快照为 0（10 分钟内）时跳过；在途计数。
- `keys.rs`：SHA-256 哈希比对、启用/过期、模型白名单、token 额度、realm 返回；错误码与文案与 Python 一致（401/403/400/429）。四类数据面路由与 `/v1/models` 全部接上鉴权。
- `chat::open_upstream`：选号 → 记录成功/失败（含冷却）→ 释放名额；realm 由密钥带入选号。
- 测试 7 例池行为 + 5 例密钥策略；全量 **58 passed**。

刻意未做：
- IP 白名单 / `max_ips`：需要 axum `ConnectInfo`（`into_make_service_with_connect_info`）才能拿到来源 IP，和 P5 的 IP 安全面板一起做。
- 用量累计（`used_tokens` / `used_credit`）：灰度期 Rust 只读 state，累加与落盘等 cutover 后由单写者接管。
- 跨进程并发闸门：进程内计数已就绪，但 Python 与 Rust 同时跑时两个进程各算一份，只有 cutover 后才等价。

**验收**：行为表（账号状态 × 请求 → 选中账号 / 冷却）由单测覆盖；与 Python 的完整双跑对比仍待 cutover 阶段。

### P5 管理面（✅ 路由层完成，6 类行为仍为 501）
已完成（2026-10-29）：
- **54 条路径 / 70 个方法**全部注册（Python 侧 admin/api 的方法级清单已对齐）。覆盖登录会话、概览、账号池、密钥、分组、出口、用户、令牌、安全/IP、审计、告警、设置、模型别名、用量、请求日志、CLI 密钥。
- 写操作统一走 `StateStore::update()` 原子落盘；只读实例返回 403（灰度期默认只读，有用例锁住）。
- 密钥与账号的敏感字段按角色裁剪：管理员才回显 `secret`，告警口令只写不读。
- 测试 8 例：登录、概览、明文密钥、密钥 CRUD、只读拒写、池设置、积分查询（mock billing）、P5 冒烟（分组 + 设置 + CLI 密钥）。

**仍返回 501（需要外部交互或加密格式，暂由 Python 提供）**：
1. OAuth 扫码：`oauth/start`、`oauth/:fid/poll`、`oauth/:fid`、`oauth/batch/start`。
2. 连接测试 `/test`（真实打上游、消耗额度）。
3. 密钥导出 `keys/:id/export`（加密导出格式）。
4. 新增账号 `POST /accounts`（凭据导入/扫码）。
5. 告警测试邮件 `alerts/test`（SMTP）。
6. 账号动作里的 `travel` / `travel-claim` / `active`（成长中心与活跃上报）。

**与 Python 的行为差异（切换前需知）**：
- 登录只支持 ADMIN_KEY 单密钥，多用户口令登录（PBKDF2 校验）尚未接入（哈希已按同格式生成，校验待补）。
- IP 白名单 / `max_ips` 未执行（缺 `ConnectInfo`）。
- `request_logs` / `metrics` / `events` 为空占位；`usage_stats` 只读快照。
- 账号库与状态写入仍受「灰度期只读」约束；cutover 后需切到可写实例。

### P6 前端与 webui 收口（进行中）
已完成（2026-10-29）：
- `src/lib/proxy-engine.ts`：引擎选择（python / rust）、端口约定（Rust 8788）、状态/启动/停止分发；Rust 走 `proxy_engine_*` invoke，Python 沿用现有命令。
- 反代页头部加「Python / Rust」切换器，切换即改 base URL 并整页重载（两套会话令牌不通用，不能热切）。
- `ServiceControl` 改为引擎感知：启动/停止/轮询按当前引擎走；Rust 下禁用「改监听地址 / 改端口 / 提权停止」，并给出原因。
- `npx tsc --noEmit` 通过；`npm test` 175 passed。

账号来源合并（2026-10-29，P6 追加）：反代不再维护「第二份账号表」，与账户管理共用同一份判据。
- **判据统一**：池选号新增两条硬过滤 —— 账号库里的 `needs_relogin`（桌面端 refresh 失败后写入）与
  `account_status[auth_invalid]`。此前只看 `access_token` 非空，桌面端已判定失效的账号仍会被派活，
  用户看到的就是「账户管理说需重新登录、反代页说可用」。池状态随之新增 `invalid`，并回报
  `needs_relogin` / `auth_invalid` / `has_token` 三个字段说明**为什么**不可用。Python 侧
  （`switch_store.account_rows` + `pool.state`）同步做了一样的事，两个引擎结论一致。
- **添加入口合并**：桌面端反代页的「扫码登录」不再走 `/admin/api/oauth/*`（Rust 仍 501），
  而是直接打开主项目的 `OAuthLoginDialog` —— 和账户管理页同一个流程，采集结果写进同一份
  `accounts.json`，两个页面立刻都看得到。webui 没有 Tauri 命令，继续用反代自己的 OAuth 链路。
  批量扫码（反代独有的服务端 slot 流程）在桌面端隐藏，避免摆一个必然失败的按钮。
- **UI 合并**：账户管理页的账号卡片直接显示反代池异常状态（冷却 / 积分耗尽 / 凭据失效；池正常时不挂徽标），
  右上角菜单新增「加入 / 移出反代池」——与反代页的暂停/恢复是同一个开关（`PATCH /admin/api/accounts/:id`）。
  反代页「账号池」降级为池运维面板：顶部加「账户管理」入口，并明确账号的增删与登录不在这里做。
- **桌面端引擎改为可写**：`proxy_engine` 用 `StateStore::open_writable`，池设置与密钥才能真正落盘；
  `StateStore::update` 写前先重读磁盘，避免与仍在运行的 Python 服务互相覆盖。灰度期仍要求只启用一个引擎。

未完成：
- **webui BFF**：`wb-switch-server` 还没有把 `/admin/api/*` 转发到本机桌面端的实现（当前 webui 仍是浏览器直连 8787 + 手动填地址/令牌）。按 D2 的计划，应加 BFF + `webui-token` 握手。
- 桌面端「应用启动即拉起 Rust 引擎」尚未默认开启（仍由用户点启动）。
- 引擎切换器与 Python 端口配置在 P7 后移除。

### P7 退场（S）
- 双跑对比 7 天（SPA 切引擎逐个页签走一遍，同请求两边比对）；删 `reverse-proxy/`、`build-exe.py`、`tauri.windows.conf.json` 的 exe resource、`verify:release` 相关检查。
- 删除「引擎」切换器与 Python 端口配置，webui BFF 的令牌文件保留。
- 更新 `CODEBUDDY.md`、`docs/DEVELOPMENT.md`、CI。
- **验收**：安装包不再含 Python 产物；`npm run tauri build` 一次通过。

---

## 7. 测试策略

| 层 | 做法 |
| --- | --- |
| 协议 | fixture 回放 + golden diff（上游请求体、SSE 帧序列、错误体） |
| 池 | 表驱动：账号状态 × 请求 → 选中账号 / 冷却 / 返回码 |
| 管理面 | 76 条路由的请求响应快照（含鉴权失败路径） |
| 安全 | 口令哈希、令牌作用域、防爆破、审计不可篡改 |
| 并发 | 与 Python 同场景压测：闸门、排队、兜底放行 |
| 跨平台 | Windows / macOS / Linux 各跑一次启动停止与端口占用 |
| webui BFF | 转发保真（方法/头/体/状态码）、桌面端离线时 503、令牌缺失/过期、并行请求 |
| 灰度切换 | 同一请求切两个引擎，响应与落库差异逐项比对 |
| 打包 | `verify:release` 增加「二进制里不得出现 python 路径」检查 |

---

## 8. 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| Responses / projection 语义漂移 | Codex 长会话失败或上下文被过度裁剪 | 先录制真实会话；灰度双跑对比上游体 |
| 流式实现差异（分帧/背压） | 客户端卡顿或半截回答 | 用帧序列测试；固定 chunk 边界回放 |
| 池行为细微差异 | 换号策略变化、额度算错 | 行为表 + 双跑日志 diff |
| 状态文件并发写 | 密钥/审计丢失 | 单写者原则 + 文件锁 + 原子替换 |
| 加密格式不兼容 | 老用户密钥解不开 | 用现有测试向量逐字节验证；只读兼容层 |
| 迁移周期长、双份维护 | 两套 bug 都要修 | P1–P5 期间 Python 只修 P0 级故障；新功能只进 Rust |
| 删 Python 后无退路 | 出问题无法回滚 | 保留一个发布周期的 feature flag（旧 exe 仍可挂） |
| webui 依赖桌面端 | 无头机器上反代彻底不可用 | BFF 明确 503 文案；后续按需加「远程模式」 |
| webui 令牌文件被读 | 本机同用户可拿管理权限 | 短期令牌 + 仅当前用户可读 + 过期即失效；不落日志 |

---

## 9. 验收标准

- [ ] 单进程：安装后没有任何 Python 运行时依赖，任务管理器只有应用本体。
- [ ] 桌面端 SPA 同源直连；webui 经 BFF 转发到桌面端，浏览器侧只有一个 origin，不需要填 base URL / 管理令牌。
- [ ] 灰度期「引擎」切换可随时回到 Python 且行为一致；P7 后切换器已移除。
- [ ] 数据面 6 条与 Python 的输出在 fixture 上逐字节一致（允许时间戳/请求 id 差异）。
- [ ] 池行为表 100% 一致。
- [ ] `state.json` 可被 Python 与 Rust 交替读写，无字段丢失。
- [ ] 灰度 7 天无差异后删除 `reverse-proxy/`，`npm run tauri build` 与 `verify:release` 通过。

---

## 10. 待确认

1. ~~管理面是否长期保留 HTTP~~ → 已定：桌面端托管 HTTP，webui 走 BFF 连本机桌面端。
2. ~~灰度双跑形式~~ → 已定：双端口（8787 / 8788）+ SPA 切换。
3. `webui-token` 的交换细节：固定路径文件 vs 桌面端写入环境变量；webui 重启后是否自动重读。
4. ~~灰度期 Rust 引擎是否允许写状态~~ → 已定：**可写**（D5 修订）。只读会让池设置/密钥的保存静默失败，比双写更难排查；
   双写风险用「写前重读磁盘 + 灰度期只启用一个引擎」兜。
5. ~~账号来源~~ → 已定：只有 `accounts.json` 一份，池是它的一层属性（D8）。
6. 是否借迁移顺手把 `wb-switch-server` 的 `/api/*`（账号切换等）也并进同一个 Router，彻底一套 HTTP 面。
7. 迁移期间新功能（例如 CLI 接入卡片）只进 Rust，还是两边都改？建议只进 Rust。
8. 扫码登录的浏览器打开方式是否沿用 `webbrowser.open`（Rust 用 `open` crate）。桌面端已改为复用主项目 OAuth，此项只剩 webui 需要。
---

## 11. 附：P5 剩余 501 的可行性

| 项 | 能否用 Rust 实现 | 关键依赖 | 风险 |
| --- | --- | --- | --- |
| OAuth 扫码（start / poll / delete / batch） | 桌面端已不需要（复用主项目 OAuth）；webui 仍需（M） | 两个上游端点 + 轮询状态机；打开浏览器用 `open` crate | 低；桌面端直接走 `wb-switch-core::oauth` 更省事 |
| `/admin/api/test` 连接测试 | 能（S） | 复用 chat 链路发一条极短请求 | 会真实消耗额度，需显式确认 |
| 密钥导出 `keys/:id/export` | 能（M） | 复刻 Python 的导出加密格式（含 settings.key） | **中高**：格式不一致会导致导入失败，必须用现有测试向量逐字节比对 |
| 新增账号 `POST /accounts` | 能（M） | 需要账号库写权限（当前 Rust 只读） | 中：会打破「账号库由桌面端/Python 单写」的约定，必须与 cutover 一起做 |
| 告警测试邮件 `alerts/test` | 能（S） | SMTP 客户端（lettre），TLS 配置 | 低 |
| `travel` / `travel-claim` / `active` | 能（M） | 成长中心与活跃上报的请求/解析 | 中：字段多、易漂移，需真实账号验证 |
| 多用户口令登录（PBKDF2 校验） | 能（S） | 哈希格式已按 Python 生成，补校验 + 会话即可 | 低 |
| IP 白名单 / `max_ips` | 能（S） | axum `ConnectInfo` | 低 |

结论：**都能迁**，没有必须依赖 Python 的项。建议顺序：
`/test` → 口令登录 + IP 白名单 → 告警测试 → OAuth 扫码 → travel/active → 密钥导出 → 新增账号（与 cutover 绑定）。

---

## 12. 结论：账号体系合并（2026-10-29）

这一节记录一次具体故障的结论，用来防止同样的分裂再长出来。

### 12.1 现象与根因

- 现象：同一个账号在**账户管理**显示「需重新登录」，在**反代页**显示「可用」，请求继续打到失效凭据上。
- 根因不是两套账号库（两个引擎读的都是 `~/.wb-switch-plus/accounts.json`），而是**两套可用性判据**：
  - 账户管理看 `accounts.json` 的 `needs_relogin`，它由 `wb-switch-core::modules::refresh` 在刷新失败时写入；
  - 反代池只看 `access_token` 非空，压根没读这一位。
- 触发源：桌面端进程继承了终端的 `HTTP(S)_PROXY`，出站被沙箱拦截（billing 域 `000`），刷新失败 → 写入 `needs_relogin`。
  去掉代理变量后同一请求返回 `401`（链路可达），刷新成功、标记被自动清除。
  **排查任何「莫名其妙要求重新登录 / 授权服务连接失败」，先确认进程环境里没有代理变量。**

### 12.2 合并后的口径

| 层 | 唯一来源 | 说明 |
| --- | --- | --- |
| 账号、凭据、登录态 | `~/.wb-switch-plus/accounts.json` | 增删改登录一律在账户管理；反代只读（Python 侧额外维护覆盖层，仅用于池内改名/启停/移除） |
| 可用性判据 | `paused → invalid → cooling → exhausted → available` | `invalid` = `needs_relogin` \| `auth_invalid` \| 无 token；Rust `pool.rs` 与 Python `pool.state()` 必须逐项一致 |
| 池运行态 | `state.json` 的 `account_status` | 冷却、积分快照、请求计数；两个引擎共用同一份 |
| 池成员与调度 | `state.json` 的 `account_overrides` + `pool` | 改名、启停、移除、路由策略、限速 |
| 池状态出口 | `GET /admin/api/pool/summary` | 每行额外给 `needs_relogin` / `needs_relogin_reason` / `auth_invalid` / `has_token`，界面据此说清「为什么不可用」 |

### 12.3 入口合并

- **账户管理页**是唯一账号入口：卡片上直接显示反代池异常状态（冷却 / 积分耗尽 / 凭据失效；正常时不挂徽标），
  菜单里「加入 / 移出反代池」与反代页的暂停/恢复是同一个开关。
- **反代页「账号池」**只剩池运维：调度策略、冷却、积分、限速、批量任务；顶部给「账户管理」入口，
  「删除」改称「移出池」（它只影响反代，不动账号库）。
- **扫码登录不实现第二套**：桌面端直接打开主项目的 `OAuthLoginDialog`，采集结果写同一份账号库；
  webui 无 Tauri 命令，继续用 `/admin/api/oauth/*`（Rust 仍 501，webui 配 Python 引擎）。批量扫码是反代独有的
  服务端 slot 流程，桌面端隐藏而不是留一个必然失败的按钮。

### 12.4 验收

- 构造 `needs_relogin: true` 的账号：账户管理与反代页都显示不可用，池选号跳过它（两侧各有单测）。
- 反代页添加账号后，账户管理列表在刷新间隔内出现同一条记录（同一份库，无同步逻辑）。
- 池设置、密钥、账号动作在 Rust 引擎下能落盘（只读实例会 403，属预期）。
