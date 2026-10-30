# 反代融合计划（v3）：管理面去 HTTP 化 —— 页面与反代同进程直连，对外只留智能体数据面

> 状态：**已实施**（P0–P4 落地于 commit `3df295b` 与 `9e42b71`，2026-10-30；验收见 §7）。P5 文档收口完成。
> 承接：`docs/reverse-proxy-integration-plan.md`（下称 v2，I0–I6 已全部完成）。
> v2 解决了「独立进程 → 内嵌、双引擎灰度 → 单 Rust 引擎、页面登录 → 宿主令牌」；
> 本文解决最后一段：**主项目页面与反代之间仍然存在的回环 HTTP**（`fetch http://127.0.0.1:<port>/admin/api/*`）。
> 约定：引用 v2 写「v2 §X」，引用本文写「本文 §X」。

---

## 0. 一句话目标

**管理面（`/admin/api/*`）不再经过 TCP 回环：前端 Tauri invoke → Rust 命令 → 进程内直接调用 axum Router（tower `oneshot`），一个字节都不落回环网卡；对外只保留数据面 HTTP（`/health`、`/v1/models`、`/v1/chat/completions`、`/v1/responses`、`/v1/anthropic/*`、`/v1/usage`）—— 这就是「对外提供智能体 API 协议接口」的全部。**

| 层 | 现状（v2 之后） | 目标（本文） |
| --- | --- | --- |
| 数据面 `/v1/*`、`/health` | 回环 HTTP（外部客户端 Codex / Claude Code 用） | **不变**。这是对外协议接口，必须留在 TCP 上 |
| 管理面 `/admin/api/*` | 页面 `fetch` 回环 HTTP + `X-Proxy-Host-Token` | **进程内调用**：`invoke("proxy_admin_request", { method, path, body })` → tower `Router::oneshot` |
| 宿主令牌 | 页面每次请求前 `proxy_admin_token` 取，放请求头 | 页面不再需要。服务端保留校验（防同机其它程序打 HTTP 管理面），但宿主自己走旁路 |

为什么值得做（不只是洁癖）：

1. **多一跳就多一类故障**。现在「服务停了 / 端口偏了 / 令牌换了」三种后端状态都会以 `fetch` 网络错误的形式砸到页面上（`reverse-proxy.ts` 里两段一模一样的 catch → "无法连接反代服务"），排查要横跨前端、IPC、TCP 三层。进程内调用后，前两类直接消失。
2. **令牌管理整个退役**。`X-Proxy-Host-Token` 的获取、401 重取、缓存失效，全是为了「页面假装成一个 HTTP 客户端」服务的。同进程调用天然可信，这一整套（约 80 行前端 + `hostauth` 在页面侧的契约）可以退场。
3. **端口偏移不再影响控制台**。v2 §3.1.1 的端口 +1 只剩一个消费者：外部客户端。控制台页面从此与端口彻底无关——哪怕 20 次重试全部耗尽、服务 `Failed`，管理面照样能用（读设置、改端口、看错误原因都走 invoke），用户永远不会再遇到「服务挂了所以打不开设置它的页面」的死循环。
4. **安全面收窄**。管理面 HTTP 从「必须监听才能被页面用」降级为「只为兼容保留的可选面」，未来甚至可以整体从 router 上摘掉（见本文 §6）。

---

## 1. 现状盘点（对照代码，2026-10-30）

### 1.1 管理面请求的实际路径

```
React 面板（21 个组件）
  → rp.<method>()            src/lib/reverse-proxy.ts:770（约 60 个端点封装）
    → request(path, opts)    src/lib/reverse-proxy.ts:116
      → getBaseUrl()         HostProvider.base()（来自 proxy_service_status 的 baseUrl）
      → getToken()           HostProvider.token()（来自 proxy_admin_token）
      → fetch(base + /admin/api/ + path, { headers: X-Proxy-Host-Token })
        ↓ 回环 TCP
      axum Router            crates/wb-switch-proxy/src/admin.rs:147-200（54 条路径 / 70 方法）
        → hostauth::authorize（回环 + 令牌头，hostauth.rs:70）
        → handler（AdminState）
```

支撑这套链路的、为目标形态要退役/改道的代码：

| 位置 | 内容 | 本文处置 |
| --- | --- | --- |
| `src/lib/reverse-proxy.ts:44-94` | `HostProvider` / `setHostProvider` / `getBaseUrl` / `setBaseUrl` / `getToken` / `setToken` | 改造（§4 J3） |
| `src/lib/reverse-proxy.ts:116-155` | `request()`：fetch + 401 重取 + 网络错误包装 | **重写为 invoke 通道** |
| `src/pages/reverse-proxy/ReverseProxyLayout.tsx:23` | `setHostProvider(...)` 注入 base/token | 删注入，改为注入「通道健康状态」 |
| `src/pages/AccountsPage.tsx:62` | `getToken as getProxyToken`（非反代页面取令牌做别用） | 逐点核查（§3.5） |
| `src/lib/proxy-service.ts:29-30, 58-60` | `hostToken` 字段 + `proxyAdminToken()` | 从 status 返回中移除；`proxyAdminToken` 删除 |
| `src-tauri/src/proxy_service.rs:46-48, 85, 104-106` | token 生成 / 下发 | 保留生成（服务端校验仍用），**停止下发给前端** |
| `crates/wb-switch-proxy/src/hostauth.rs` | 宿主令牌校验器 | **保留**（外部 HTTP 打管理面的唯一闸门），页面侧契约退役 |

### 1.2 服务端已有条件（对本文有利的部分）

- `admin.rs` 的 router 是纯 `(State, Request) -> Response` 的 axum 形态，`AdminState::from_env()` 独立构造、`with_admin()` 可外部注入（`http.rs:75`）——**把它当普通 tower Service 在进程内 call 没有任何结构障碍**。
- 管理面 70 个方法**全部是 JSON 请求/响应，无 SSE、无流式、无二进制**（连接测试 `POST /test` 的上游流已被服务端聚合）。进程内通道不需要处理流式回传，一条 `serde_json::Value` 进出即可。
- `ProxyService`（`src-tauri/src/proxy_service.rs`）已持有 `ProxyHandle`，且 `start_with_auth` 接受外部 `HostAuth` —— 在这里同时持有 `Arc<AdminState>` 是自然延伸。
- 数据面（`chat.rs` / `responses.rs` / `anthropic.rs` / `wire.rs` / `projection.rs`）本次**一行不动**（延续 v2 D7 的纪律）。

### 1.3 为什么数据面不能也不该进进程内

外部客户端（Codex CLI、Claude Code、OpenAI SDK 生态）是**独立进程**，它们与反代的契约就是 HTTP + SSE。数据面留在 TCP 上不是妥协，是产品本身：**「对外提供智能体 API 协议接口」指的就是这组监听在 `127.0.0.1:<port>` 的 `/v1/*`**。本文所有改动都发生在管理面，数据面的鉴权（API key）、池调度、协议转换不进入讨论范围。

---

## 2. 目标形态

```
┌──────────────── Tauri 桌面端（单进程）────────────────┐
│                                                       │
│  WebView (dist/)                 Rust 后端            │
│  ┌──────────────────┐   invoke    ┌──────────────────┐│
│  │ 反代控制台页面    │ ──────────→ │ proxy_admin_request ││
│  │  (不再 fetch)    │ ←────────── │   ↓ tower oneshot  ││
│  └──────────────────┘   JSON 值   │ axum admin Router  ││
│                                   │   (AdminState)     ││
│                                   ├──────────────────┤│
│                                   │ ProxyService       ││
│                                   │  └ ProxyHandle ────┼│── 127.0.0.1:8787
│                                   │     数据面 axum     ││   /health /v1/*  ← 外部客户端
│                                   └──────────────────┘│
└───────────────────────────────────────────────────────┘
```

三条不变量：

1. **数据面不动**：`/v1/*` 仍是回环 HTTP + SSE，端口策略（v2 §3.1.1）原样生效，外部客户端无感。
2. **管理面 router 不重写**：`admin.rs` 的 54 条路由、handler、`AdminState` 全部保留，只是被两个入口调用——HTTP（保留，供排障/外部工具）与进程内（新，页面专用）。
3. **「反代服务old」不受影响**：Python 页面继续走它自己的 HTTP（8788 + `X-Admin-Token`），本文不碰 `reverse-proxy-service.ts` 与 `LegacyReverseProxyPage` 的传输层。

---

## 3. 设计

### 3.1 进程内通道：tower `oneshot`，不是「命令逐个重写」

**不做**：为 60 个管理端点在 Rust 里各写一个 Tauri 命令（`proxy_overview` / `proxy_list_keys` / …）。那是 60 份重复的类型定义与同步维护负担，而且 admin router 本身就是一份已经过 45 个测试验证的契约。

**做**：一条通用通道命令，把 HTTP 语义原样搬进 IPC：

```rust
// src-tauri/src/proxy_service.rs 新增（形状示意）
/// 请求：{ method, path, body }
/// 响应：{ status, body }  —— body 已是反序列化过的 serde_json::Value
pub async fn admin_request(&self, method: String, path: String, body: Option<Value>)
    -> Result<AdminReply, String>
{
    let router = self.inner.lock().unwrap().admin_router.clone()?; // Arc<Router>
    let request = build_request(method, path, body)?;              // 补 JSON 头
    let response = router.oneshot(request).await
        .map_err(|e| format!("管理面调用失败：{e}"))?;
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), MAX_ADMIN_BODY).await?;
    Ok(AdminReply { status, body: serde_json::from_slice(&body).unwrap_or(Value::Null) })
}
```

关键取舍：

1. **沿用 HTTP 状态码作为错误语义**。`status >= 400` 时 Tauri 侧**不**返回 Err，而是把 `{status, body}` 原样给前端——让现有 `RpError` 的 `status` 字段与各面板的 4xx 分支逻辑（如 keys 面板的 409 提示）零改动继续工作。只有「服务未启动 / 通道本身坏了」才是 Err。
2. **router 的持有方式**：`ProxyService::start()` 时 `AdminState::from_env()` + `admin::router().with_state(...)` 构建，`Arc<axum::Router>` 存进 `Inner`；`stop()` 时清空。这样服务停机时进程内通道**如实报错**（Err："内置服务未运行"），页面显示现有的「内置服务已停止」状态——而不是假装能查。
3. **`ConnectInfo` 与 hostauth 的旁路**：进程内调用没有 peer 地址。在 `build_request` 里**显式插入一个 `PeerLoopback` extension**（新类型，定义在 `hostauth.rs`），`admin.rs` 的鉴权中间件识别到它就按 admin 放行。**不要**伪造 `127.0.0.1` 的 `SocketAddr` 塞进 `ConnectInfo`——那会让「进程内调用」和「真回环 HTTP」在代码里无法区分，排障时会骗人。
4. **hostauth 保留在线上**：HTTP 入口的管理面仍要求 `X-Proxy-Host-Token` + 回环。区别只是它的消费者从「自家页面」变成「未来可能的排障脚本 / 外部工具」。`WB_PROXY_HOST_TOKEN` 环境变量（`hostauth.rs:41`）继续是它的开启方式；桌面端启动时仍生成随机 token，只是**不再告诉前端**。
5. **body 上限**：`MAX_ADMIN_BODY` 定 8 MB（request-logs / audit-logs 大分页的最大观测值再留 4 倍余量），超限直接 Err，防患于未然。

### 3.2 一个必须想清楚的点：锁与并发的形状

现在 `ProxyService.inner` 是一把 `Mutex`，`admin_request` 若在持锁状态下 `await oneshot`，会把 start/stop/status 全部卡在一个慢查询后面。规则：

- **取 `Arc<Router>` 用短锁**（clone 后立刻 drop guard）；
- `oneshot` 在锁外执行；
- `AdminState` 内部自有的并发原语（`state.rs` 的 `StateStore` 已是写前重读 + 原子写）不受影响。

### 3.3 前端传输层改造：`request()` 换引擎，签名不动

`src/lib/reverse-proxy.ts` 是全站反代面板的唯一 API 层（23 个引用点全部经 `rp.*` 或类型导出）。改造策略：**只换 `request()` 的内部实现和 HostProvider，60 个 `rp.*` 方法、全部类型、全部组件零改动**。

```ts
// 改造后（形状示意）
type AdminChannel = {
  call: (method: string, path: string, body?: unknown) => Promise<{ status: number; body: unknown }>;
};
let channel: AdminChannel = { call: () => Promise.reject(new RpError("通道未就绪", 0)) };
export function setAdminChannel(next: AdminChannel | null): void { ... }  // 取代 setHostProvider

async function request<T>(path, options): Promise<T> {
  const { status, body } = await channel.call(options.method ?? "GET", path, options.body);
  if (!ok(status)) throw new RpError(extractDetail(body) || `请求失败（HTTP ${status}）`, status);
  return body as T;
}
```

逐项清理：

| 项 | 处置 |
| --- | --- |
| `HostProvider` / `setHostProvider` / `getToken` / `setToken` | 删。`setHostProvider` 的唯一调用点是 `ReverseProxyLayout.tsx:23`，改为 `setAdminChannel`（内部 `invoke("proxy_admin_request", …)`） |
| `getBaseUrl()` / `setBaseUrl()` / `DEFAULT_BASE` | `getBaseUrl` 有两个真实消费者：`cli-link-card.tsx:10`、`service-control.tsx:7`（old 页）。两者要的是**数据面地址**（给外部 CLI 看的 `http://127.0.0.1:<port>`）——改从 `proxy_service_status` 的 `baseUrl` 直取，`reverse-proxy.ts` 里这三个导出删除。`reverse-proxy-service.ts:3` 的引用随 old 页面改为本地常量 8788 |
| 401 重取逻辑（`request()` 两段 catch + 401 重试） | 删。进程内调用不存在令牌过期；401 只可能来自未来某个带鉴权的外部场景，与本通道无关 |
| `RpError` 的 `status` 分支 | 保留（§3.1 第 1 条），`status === 0` 的语义改为「通道不可达（服务未启动）」 |
| `login` / `loginKey` / `loginToken` / `loginOptions` / `logout` / `revokeSessions`（`rp.*` 前 6 个） | **保留但不被内置页面调用**。它们服务 old 页面（Python HTTP）的路径不变；router 里的同名端点也保留（HTTP 入口仍是完整契约） |
| `apiBaseFromService()` / `modelsEndpoint()`（`:1040`/`:1044`） | 保留——它们拼的是**数据面**地址，本来就与本文改动正交 |

### 3.4 `ProxyServiceStatus` 的字段变化

| 字段 | 处置 | 理由 |
| --- | --- | --- |
| `baseUrl` / `port` / `desiredPort` / `portShifted` / `portNotice` | **保留** | 数据面地址仍要展示在状态条、CLI 接入卡、接入指南里 |
| `hostToken` | **移除** | 前端不再需要；少一个在 IPC 里频繁搬运的敏感值 |
| `running` / `phase` / `error` | 保留 | 通道 Err 时页面靠它区分「服务停了」还是「调用坏了」 |

`proxy_admin_token` 命令删除（Tauri `invoke_handler` 同步摘除）。若 `AccountsPage.tsx:62` 的 `getProxyToken` 有独立用途（它是全站唯一在反代页之外取令牌的点），J3 阶段单独核查：用途若为「给反代数据面当 key」则是误解令牌语义，应改为真正的数据面 API key；用途若为管理面则随通道退役。

### 3.5 old（Python）页面的边界

old 页面 `LegacyReverseProxyPage` 与其依赖（`reverse-proxy-service.ts`、`service-control.tsx`、登录表单）**完全不进本文范围**。唯一交叉点：`service-control.tsx` 引用了 `getBaseUrl()`（§3.3 已列）——改成读 old 自己的 base（8788 或其页面状态里的实际端口），顺手让 `reverse-proxy.ts` 与 Python 启动器解耦，这是一次该做的反向依赖清理。

### 3.6 兼容性：HTTP 管理面保留为「排障面」

HTTP 入口（`/admin/api/*` + hostauth）**不删**：

- `bin/wb-proxy-dev.rs` 的开发流程、`tests/admin.rs` 的 45 例集成测试全部依赖真实 HTTP；
- 未来排障（`WB_PROXY_HOST_TOKEN=… wb-proxy-dev` + curl）需要它。

但语义降级：文档与代码注释明确「HTTP 管理面是开发/排障通道，产品 UI 走进程内通道」。v2 §3.3 那套「宿主注入」论述由本文 §3.1 取代，`hostauth.rs` 的 doc 注释同步改写（它现在描述的「前端经 invoke 取令牌放请求头」已不是事实）。

---
## 4. 分阶段实施：P0–P5（本节为唯一权威切分；正文 J1-J3、附录 A4 / C.3 / D.2 / B.2 的 commit 归属全部由本节取代）

> 拆分原则：**P0 全部是删除（行为收敛，风险最低、收益立现）→ P1 打通进程内通道（核心）→ P2 前端切换（依赖 P1）→ P3 收尾清理（依赖 P2）→ P4 新功能（局域网，独立）→ P5 文档收口**。
> 每个 P 都是完整可交付单元：独立编译、独立测试、独立回滚。附录 A 的 A1.x/A2.x 步骤明细、B/C/D 的技术要点**继续有效**，只是归属被本节重新编排。
>
> ⚠️ 与附录旧归属的差异由 P 优先级驱动：**P0 先删后建**——先摘掉死功能与管理面 HTTP，`admin.rs` 瘦身之后再打通道，P1 的实现面更小、测试基线更干净。这与 A4「先 InProcess 后删除」的顺序相反，以本节为准。

### P0 管理面 HTTP 摘除 + 死功能删除（Rust，1 天）

**目标**：反代对外只剩数据面；admin router 内部瘦身完毕，但**此时前端仍走 HTTP**——所以 P0 的摘除必须与 P2 前端切换在同一次发版内完成，不能单独发布（P0-P2 是一个发版单元，commit 可独立但发布绑定）。

| # | 内容 | 来源 |
| --- | --- | --- |
| P0.1 | `http.rs` 去 `.nest("/admin/api")`、去 CORS 层；数据面 router 成为唯一 HTTP 面 | C.2.1 |
| P0.2 | `admin.rs` 摘除：session/login/logout/revoke/login-options（6 条）、users（3 条）、tokens（4 条）、audit-logs（2 条）+ 全部对应 handler 与 session helper | B.2 + C.2.4 |
| P0.3 | `AppState` 删 `admin: Arc<AdminState>`、`host_auth` 字段；`AdminState` 结构与 `hostauth.rs` 的令牌体系（HostAuth/authorize/HOST_TOKEN_HEADER/from_env/random_token/constant_time_eq）删除；`peer_from_connect`/`peer_ip` 保留（数据面白名单用） | C.2.2-3 |
| P0.4 | `ProxyService` 删令牌生成/持有（`host_token()`、`HostAuth::with_token` 调用点）；`status()` 删 `hostToken` | C.2.3 |
| P0.5 | `wb-proxy-dev` 改纯数据面（删令牌打印；`--port/--data` 保留） | C.2.6 |
| P0.6 | 测试：118 例中 admin 会话/users/tokens/audit 用例删除；新增负向断言「HTTP `/admin/api/*` → 404」；数据面白名单矩阵保留 | C.1 |

**验收**：`cargo test -p wb-switch-proxy` 全绿；`curl /admin/api/overview` → 404；`/health`、`/v1/*` 带白名单密钥正常；`wb-proxy-dev` 跑通白名单/密钥/usage/连接测试矩阵。
**注意**：P0 落地后到 P2 之前，内置控制台页面不可用（fetch 404）——**这是预期中间态**，P0-P2 同一发版单元。

### P1 进程内管理面通道（Rust，0.5 天）

| # | 内容 | 来源 |
| --- | --- | --- |
| P1.1 | `build_state(cfg, store) -> Arc<AppState>`（无 auth 参数，P0 已删）；`ProxyHandle::bind` 改收外部 Arc；`start_with_state` 变体 | A1.2 |
| P1.2 | `ProxyService::Inner` 加 `admin_router: Option<Router<()>>`（`admin::router().with_state(state)`，P0 瘦身后的形态）；start 装配 / stop 同锁清空 | A1.3 |
| P1.3 | `admin_request(method, path, body)`：tower `oneshot`；**无 InProcess extension、无任何鉴权组装**（C.2.3：admin router 摘出 HTTP 后进程内是唯一入口，天然可信）；4xx/5xx 走 `Ok({status, body})` 透传；路径前缀在 Rust 侧不存在（前端传相对路径）；8MB 上限；path 清洗；锁外 await | A1.3 + C.2.3 |
| P1.4 | Tauri 命令：注册 `proxy_admin_request`（`Ok = {"status", "body"}`，`Err = 通道故障`）；`proxy_admin_token` 已在 P0.4 随令牌删除 | A1.4 |

**验收**：新增单测 ~6 例——overview 200、`POST /test` mock 上游、4xx 透传、服务未启动 Err、stop 后 Err 不 panic、并发与 start/stop 无死锁。
**依赖**：P0。

### P2 前端切换（TS，1 天）——P0+P1+P2 必须同发版

| # | 内容 | 来源 |
| --- | --- | --- |
| P2.1 | `reverse-proxy.ts`：`request()` 换通道引擎（A2.1 代码块原样适用）；删 HostProvider 系、`getBaseUrl`/`setBaseUrl`/`DEFAULT_BASE`、401 重取；**`rp.login*` 7 个方法直接删**（B 节连带收敛） | A2.1 + B |
| P2.2 | 内置页死功能 UI 摘除：删 `TokensPage`/`UsersPage`/`AuditPage` 薄壳 + 路由（`App.tsx:408` 等）；`nav.ts` 高级组只剩 `security`、`count` 枚举删 `"tokens"|"users"`；`security-panel.tsx` 删审计子 Tab（:187-223）；`rp.listTokens/listUsers/auditLogs` 等 10 个方法与 `RpToken/RpUser/RpAuditLog` 类型**保留**标注 `@deprecated 仅 old`（B.2.1 的修正结论） | B.2 |
| P2.3 | `ReverseProxyLayout.tsx`：`setHostProvider` → `setAdminChannel` 一次装配（A2.2 的语义变化与两点补偿照做）；`proxy-service.ts` 删 `proxyAdminToken`/`hostToken` | A2.2 + A2.4 |
| P2.4 | base 消费者迁移：`cli-link-card.tsx` 改 props 传 baseUrl；`service-control.tsx` / `reverse-proxy-service.ts` 改本地常量 8788 | A2.3 |
| P2.5 | `AccountsPage.tsx`：删 `getProxyToken` 判断（:62、:515），直接 `rp.poolSummary()` | A2.5 |

**⚠️ P2.1 + P2.3 必须同一 commit**（A4 的论证不变：分开必产生不可用中间态）。
**验收**：DevTools `/admin/api/*` fetch 0 条；21 面板逐页比对；停机→启动恢复无刷新；`npm test` / `tsc --noEmit` 全绿。

### P3 测试迁移与残留核查（TS/Rust 收尾，0.5 天）

| # | 内容 | 来源 |
| --- | --- | --- |
| P3.1 | `reverse-proxy.test.ts`：stubFetch → `setAdminChannel` 桩（A2.6 代码块）；断言 url → path；桩数据按 `02cb5bf` 后契约；新增 409/reject 两例 | A2.6 |
| P3.2 | `rp-013.test.ts` audit 用例删（:119-134）；`proxy-service.test.ts` hostToken 桩字段删 | B.2 |
| P3.3 | grep 残留核查：`proxyAdminToken|proxy_admin_token|setHostProvider|getBaseUrl|hostToken|DEFAULT_BASE` 在 src/ 与 src-tauri/ 归零；`HOST_TOKEN_HEADER|ADMIN_KEY` 在 proxy crate 归零（state.json 键名与 old 注释除外） | A3 + C.4 |
| P3.4 | `site_name` 清理：`RpServiceSettings`/`RpSettingsPatch` 删字段、`reverse-proxy.test.ts:291-309` 两例删（grep 确认无 legacy 引用） | D.1 |

**验收**：全量 `cargo test` / `npm test` / `tsc --noEmit` / `npm run tauri build`。

### P4 内置服务卡片 + 局域网开放（Rust + TS，1 天，可独立发布）

| # | 内容 | 来源 |
| --- | --- | --- |
| P4.1 | `config.rs`：host 读取顺序 `WB_SERVE_HOST` → `settings.reverse_proxy.host` → 127.0.0.1 | D.1 |
| P4.2 | `proxy_service.rs`：`saved_host`；`status()` baseUrl 去硬编码 + `host`/`lanExposed` 字段；`set_bind(host, port)` 命令（一次重绑） | D.1 |
| P4.3 | `service-settings-panel.tsx` 重组为内置服务卡片：监听范围开关 + 端口 + 数据目录 + 启停；局域网开启黄条提示「白名单为空的密钥全网段可用」 | D.1 |
| P4.4 | `proxy-service.ts`：`host`/`lanExposed` 字段、`proxyServiceSetBind` | D.1 |

**验收**（D.2）：局域网设备带白名单密钥 `curl /v1/models` 200 / 白名单外 403；管理面仍 404；关掉恢复本机。
**依赖**：P2（前端卡片走通道）与 P3.4（site_name 清空后重组面板）。P4.1/P4.2（Rust 侧）可与 P0-P1 并行开发，发布仍绑定在 P2 之后（P4.3 的卡片调用 `rp.*` 走通道）。

### P5 文档与发布（0.5 天）

- `hostauth.rs` doc / `proxy_service.rs` 模块注释 / `CODEBUDDY.md` / `DEVELOPMENT.md`：按「数据面 = 唯一 HTTP 面；管理 = 进程内」改写；`hostauth.rs → peer.rs` 改名（C.2.2 可选项）；
- v2 文档顶部指向本文；发布说明按 C.2.7（安全净效果）+ B 节（功能移除）+ P0（管理面 HTTP 消失）；
- 全量 CI。

---

### P 拆分与旧归属的映射（检索用）

| 旧编号 | 新归属 |
| --- | --- |
| J1 / A1.1（InProcess + authorize 三分支） | **取消**（C.2.3）；P0 摘除后无需旁路 |
| J1 / A1.2（build_state） | P1.1 |
| J1 / A1.3+A1.4（通道 + 命令） | P1.2-P1.4 |
| A1.5 测试 #3-#5（四象限） | 取消（C.1）；改 P0.6 负向断言 |
| J2 / A2.1-A2.5 | P2.1-P2.5 |
| J3 / A3 | P3.3 + P5 |
| A4 commit 1（当时=InProcess） | P0（内容大幅扩大：管理面 HTTP 整体摘除） |
| A4 commit 2-6 | P1 / P2 / P3 / P5 |
| B.2 各行 | P0.2（Rust 摘除）、P2.2（前端 UI）、P3.2（测试）、B.2.1 的方法保留结论进 P2.1 |
| C.3 commit 1-6 | P0（=C.3 commit1+2 合并）→ P1（=commit3）→ P2（=commit4-5）→ P3/P5（=commit6） |
| D.2 | P4.1-P4.4（Rust 侧可与 P0-P1 并行开发） |
---

## 5. 测试策略（增量）

| 层 | 做法 |
| --- | --- |
| 通道单测（Rust） | `admin_request`：200/404/400 透传；服务未启动 Err；body 超 8MB Err；并发 50 个请求不与 start/stop 死锁（`tokio::test` + race） |
| 鉴权回归（Rust） | HTTP 面带/不带令牌、回环/非回环 四象限用例（现有 hostauth 测试扩展 `PeerLoopback` 不泄漏到 HTTP 面的断言） |
| 前端传输 | `request()` 桩：4xx → `RpError.status`；5xx → 同；通道 reject → `status: 0`；`extractDetail` 各形态 |
| 手工清单 | DevTools 零 fetch 核查；停机→启动恢复；端口偏移时控制台仍可用（这是本架构的直接红利，必须验证）；old 页面全功能回归 |

---

## 6. 风险与遗留

| 风险 | 缓解 |
| --- | --- |
| 进程内调用绕过鉴权，被误当成「管理面不再需要鉴权」 | `PeerLoopback` 是显式新类型，只在 Tauri 命令构造请求时插入；HTTP 面四象限测试钉死闸门 |
| `admin_router` 的生命周期与 `ProxyHandle` 不同步（stop 后 router 还活着） | `stop()` 同一把锁里同时清空两者；通道在 router 为 None 时立即 Err |
| 未来某个管理端点引入 SSE/流式，进程内通道静默坏掉 | 在 `admin.rs` 顶部注释立约：新增管理端点必须 JSON-in/JSON-out，否则同步改造 `admin_request`；本文写进 CODEBUDDY.md |
| 通用通道让前端可以打任意管理路径，弱化了「60 个方法」的类型边界 | 可接受：这些方法本来就全在 `rp.*` 里被类型化封装，组件不会裸调 `channel.call`；通道仅是传输层 |
| 遗留：`login*` 端点与 `AdminState.sessions` 在内置页面侧成为死代码 | 保留（old 与 HTTP 排障面仍用）；Python 整体移除时（v2 §3.4 退场条款）一并评估 |

**明确不做**：数据面进程内化（§1.3）；old 页面改造；`wb-switch-proxy` 的 crate 结构调整；协议层任何改动（延续 v2 D7）。

---

## 7. 验收标准（P0–P4 已落地，commit `3df295b` / `9e42b71`；勾选含自动化依据）

- [x] DevTools Network：反代控制台全站操作，`/admin/api/*` 的 HTTP 请求为 0 条（前端已无任何对该地址的 fetch——`request()` 走通道，自动化由 177 例前端测试与通道桩覆盖）。
- [x] 服务停机时反代页面可用（能启动、能看错误原因）：通道恒装配，停机由 `phase` 分支渲染「已停止 + 启动」。
- [x] 端口偏移（+1 / 20 次耗尽 `Failed`）时控制台功能完整（通道与端口无关），数据面地址按 v2 §3.1.1 提示。
- [x] 外部客户端零感知：数据面协议层一行未动（P0 只摘管理面），`/health`、`/v1/*` 行为与改造前一致（skeleton/chat/catalog 测试回归）。
- [x] HTTP 上不存在管理面：`/admin/api/*` 任何路径 → 404（`tests/admin.rs` 负向断言 × 5）。
- [x] `cargo test`（proxy 108 + tauri lib 54）/ `npm test`（177）/ `npx tsc --noEmit` / `vite build` 全绿。
- [x] `proxy_admin_token` 命令、`hostToken` 字段、前端 401 重取逻辑、`X-Proxy-Host-Token` / `ADMIN_KEY` 在代码中不存在（P3.3 grep 归零）；文档已按「数据面 = 唯一 HTTP 面；管理 = 进程内」改写。
- [x] 局域网开放（附录 D）：`set_bind` 白名单两值、持久化、`lanExposed` 字段与 UI 开关 + 白名单联动提示（自动化：`saved_host` 白名单单测；双机 curl 矩阵待真机验收）。
- [x] 「管理令牌 / 后台账号 / 审计」功能移除（附录 B）：内置页签与 Rust 端点删除，old 页三面板经 `legacy-admin.ts` 原样保留。
- [x] `site_name` 退役（附录 D）：类型、测试、面板表单摘除。

**待真机验收（不阻塞发版）**：局域网双机 curl 矩阵（白名单内 200 / 外 403 / admin 仍 404）；`wb-proxy-dev` 全矩阵复跑。

---

## 修订记录（结合 git 迭代，2026-10-30 下午审阅）

本文初稿基于 `bf0061c` 时的代码。初稿提交（`d481338`）之后、本节编写之前，仓库又进了 5 个提交，逐一审阅对本文的影响：

| 提交 | 内容 | 对本文的影响 |
| --- | --- | --- |
| `a861be5` | 管理面加 CORS 层（tower-http `CorsLayer`，任意 origin + 宿主令牌头放行），修复 `tauri.localhost` 跨源被浏览器拦下导致「无法连接反代服务」 | **是本文论点的直接实证**：正文 §0 第 1 条说「多一跳就多一类故障」，这个提交就是那类故障的实锤（服务端日志正常、前端全连不上）。J2 落地后管理面跨源请求归零，这层 CORS 失去主要消费者；**处置：保留不删**——HTTP 排障面（wb-proxy-dev + 浏览器直开）仍需要它，对数据面无害。§3.6「HTTP = 排障面」语义把 CORS 一并覆盖。 |
| `f4185df` | 新增 `metrics.rs`（数据面四条路径打点，对齐 Python MetricsMiddleware），`AppState` 增加 `metrics: Arc<Metrics>` | 不影响方案；影响 A1.2：`AppState` 构造权上提必须连 `metrics` 字段一起搬——`build_state` 成为唯一构造点后，字段增减只改一处，恰好验证了上提的价值。进程内通道不打数据面指标，无交互。 |
| `d2db706` / `02cb5bf` | admin 端点形状对齐 Python（日志键类型、overview.groups 数组化、groups/egress/model-aliases/models 四端点） | 前端 `rp.*` 的桩数据与断言以**修复后形状**为准；A2.6 迁移测试时不得照抄旧用例的旧形状。 |
| `c00b94c` | 密钥 IP 白名单写入/回显补齐；新增 `src/bin/wb-proxy-dev.rs`（本地 curl 验证入口，`--port/--data` 参数化、打印宿主令牌、独立端口 8790） | ① v2 删 `bin/wb-proxy.rs`（正式入口）的决策不变；`wb-proxy-dev` 是新性质的 bin——纯本地验证、不随安装包分发，本文 §3.6 的「HTTP 排障面」由它承载，J3 把它写进排障指引。② 它与桌面端 8787 完全隔离，进程内通道改造对它零影响；A1.6 验收复用它的 curl 矩阵。 |
| `8ebb74a` | IP 白名单真正生效（keys.rs `ip_allowed`：精确/CIDR/跨族回环；数据面四 handler 接 `ConnectInfo`，403 `ip_not_allowed`）；v2「IP 安全延后」遗留项**关闭** | ① 正文遗留表述更新（见下）。② `hostauth.rs` 新增 `peer_from_connect`（`Option<ConnectInfo>` 包装，oneshot 测试无连接信息不 500）——与 A1.1 旁路设计同源但语义相反：数据面对 `None`（未知来源）**放行**，管理面对 `None` **拒绝**（`hostauth.rs:84-85`）；进程内调用没有 peer，所以 `InProcess` extension 是管理面唯一显式旁路，且不得复用「None 放行」路径。 |

测试基线更新：初稿写「45 例」，`8ebb74a` 后为 **118 例**（含 12 例 IP 白名单）；A1 验收按 118 例 + 新增 8 例计。

**正文随迭代修正两处**：

1. §1.1 语境补注：现存在 `bin/wb-proxy-dev.rs`，性质是本地验证工具；「删除正式独立入口」的决策不变。
2. §6 遗留清单更新：「~~IP 白名单~~（`8ebb74a` 已关闭）→ 剩余遗留：密钥导出、告警测试邮件（仍走 old）」。v2 §10 遗留清单以本表为准。

---

## 附录 A. 详细步骤与技术要点（供审阅；行号以 `02cb5bf` 时代码为准）

### A.0 核对代码后对正文的两处修正

1. **§3.1 第 2 条「ProxyService::start() 时构建 admin router」不可行，需上提 AppState 构造权。**
   现状：`AppState::new()` 只在 `ProxyHandle::bind` 内部调用（`crates/wb-switch-proxy/src/lib.rs:119`），admin router 经 `http::router(state)` 的 `.nest("/admin/api", admin::router())`（`http.rs:62`）嵌在数据面 router 里，`ProxyService` 对它没有引用。
   修正：新增公开函数 `build_state(cfg, store, auth) -> Arc<http::AppState>`（内容即现在的 `Arc::new(AppState::new(...))`），`ProxyHandle::bind` 改为接收外部传入的 `Arc<AppState>`（新增 `start_with_state` 变体）；`ProxyService::start()` 先 `build_state` 再启动；进程内通道用 `admin::router().with_state(state.clone())` 单独组装 admin 子 router（未经 nest，URI 不带 `/admin/api` 前缀——见 A1.3 要点 1）。
2. **§3.1 第 3 条「旁路 extension」从「建议」升级为「必需」。**
   现状：`hostauth.authorize` 在 `peer == None` 时返回 false（`hostauth.rs:84-85`），进程内 `oneshot` 调用天然没有 `ConnectInfo`，只带令牌头也会被拒。另见修订记录 `8ebb74a` 行：数据面 `None` 放行、管理面 `None` 拒绝，`InProcess` 是管理面唯一显式旁路。

### A1. J1 详细步骤（Rust）

**A1.1 `hostauth.rs`：新增进程内身份标记（~30 行 + 测试）**

```rust
/// 进程内调用的身份标记。只由宿主（Tauri 命令层）在构造请求时插入 request 的
/// extensions；HTTP 面永远不会有它 —— axum serve 不会凭空造 extension。
pub struct InProcess;
```

- `admin.rs` 的 `authorize()`（`admin.rs:133`）改为三分支：
  1. 请求带 `InProcess` extension → 直接返回 `("in-process", "", "admin")`（不查头、不查 peer）；
  2. 否则走现有 `host_auth.authorize(headers, peer)`；
  3. 否则回退 `session_of(...)`。
- 技术要点：
  - `authorize` 现签名是 `(state, headers, peer)`（`admin.rs:133`），拿不到 extensions。改法二选一：(a) 签名加一个 `&Extensions` 参数，调用方传 `request.extensions()`；(b) 鉴权收敛成 `axum::middleware::from_fn`。**实施时先核实 54 个 handler 调 `authorize` 的形态**（内联调用还是 extractor）；内联居多则选 (a) 最小侵入；(b) 更干净但改动面是 54 个 handler，属顺手重构，**不建议与本次混做**。
  - `InProcess` 定义在 `hostauth.rs`（管理面鉴权语义集中一处）；
  - 单测钉死：带 `InProcess` → admin 放行；去掉 extension、带正确令牌头、无 peer → 401（证明标记不「漏」——HTTP 面的 extension 只能来自显式插入）。

**A1.2 `lib.rs`（proxy crate）：上提 router 构造（~40 行重构，118 例断言零变化）**

- 新增 `pub fn build_state(cfg, store, auth) -> Arc<http::AppState>`；
- `ProxyHandle` 增加 `state: Arc<http::AppState>` 字段与 `pub fn state(&self) -> &Arc<http::AppState>`；`bind`（`lib.rs:110-134`）改收外部 Arc；保留 `start_with_auth`（内部先 build_state），新增 `start_with_state`；
- 要点：
  - `AppState` 现含 `metrics` 字段（`f4185df`），构造权上提后字段增减只改 `build_state` 一处；
  - `ProxyHandle::stop()` 消费 `self`（`lib.rs:162`），但 `Arc<AppState>` 已被 `ProxyService` 另持一份，生命周期互不影响；stop 后 Arc 残留无害（无监听、无后台任务，metrics 只在数据面 handler 打点）；
  - 纯重构：`bind` 行为不变，118 例断言零变化是它的验收标准。

**A1.3 `proxy_service.rs`：通道实现（~90 行 + 单测）**

- `Inner` 增加 `admin_router: Option<AxumRouter>`，类型是 `admin::router().with_state(state.clone())` 之后的 `Router<()>`（已 state 化，可直接 oneshot）；
- `start()` 成功分支组装并存入；`stop()`（:196-209）与 `stop_blocking()`（:214-239）在**取 handle 的同一临界区**里清空（两处都要加）；
- 新增 `admin_request`：

```rust
pub async fn admin_request(&self, method: String, path: String, body: Option<Value>)
    -> Result<AdminReply, String>
// AdminReply { status: u16, body: Value }
```

技术要点（逐条）：
1. **路径前缀只存在一处**。A1.2 组装的是裸 admin router（未经 nest），通道直接用 `/{path}` 打它（前端传 `overview` 这类相对路径，与 `rp.*` 写法一致）；前端 `request()` 里现有的 `"/admin/api/" + path`（`reverse-proxy.ts:118`）随 J2 删除——拼接逻辑从此不存在，而不是从 TS 搬到 RS。
2. **请求构造**：`Request::builder().method(method).uri(path)`；body 非 null 时加 `content-type: application/json` + `Body::from(serde_json::to_vec(...))`；GET/DELETE 不带 body；`.extension(hostauth::InProcess)` 必加。
3. **锁纪律**：`let router = self.inner.lock().unwrap().admin_router.clone();` guard 立即释放，`oneshot` 在锁外 await。否则一个慢查询（request-logs 大分页）会卡死 `status()`/`stop()`/`set_port()`。
4. **状态码语义**：`status < 400` → `Ok(AdminReply{status, body})`；`>= 400` **同样 Ok 返回**，由前端 `request()` 判 status 抛 `RpError`（keys 面板 409 分支、白名单 403 `ip_not_allowed` 文案分支都零改动）。Rust 侧 Err 只留给三类通道故障：router 为 None（服务停机）、请求构造失败、body 超限——Tauri 把 Err 变成 rejected promise，前端 catch 后抛 `RpError("...", 0)`，与现有「连接失败」语义对齐。
5. **body 上限**：`axum::body::to_bytes(body, 8 * 1024 * 1024)`，超限 Err。8MB 对 request-logs / audit-logs 大分页有 4 倍余量。
6. **path 清洗**（纵深防御）：拒绝 `..`、`//`、非 `^[a-z0-9][a-z0-9\-/]*$`；路由匹配本身是第二道闸，这层只防前端 bug 打出意外 URI。
7. **`status()` 移除 `hostToken` 字段**（:85）；`host_token()` 降为 `pub(crate)`（`start()` 组装 `HostAuth::with_token` 仍要用，不再暴露给前端）。
8. **stop 与在途请求的竞争**：clone 到 router 后 stop 清空，在途请求仍执行完（axum 无取消传播）。无害：响应照样回来，下一发才 Err。不加 generation counter 之类复杂度。
9. **CORS 与进程内通道无关**（`a861be5`）：CORS 层挂在 `http::router`（完整 router）上，A1.2 的裸 admin router 不含它——进程内调用没有浏览器预检问题，也不需要。

**A1.4 `commands.rs` + `lib.rs`（Tauri 壳）：命令增删（~25 行）**

- 删 `proxy_admin_token`（`commands.rs:120-125`）；
- 增：

```rust
#[tauri::command]
pub async fn proxy_admin_request(
    state: tauri::State<'_, crate::proxy_service::ProxyService>,
    method: String, path: String, body: Option<Value>,
) -> Result<Value, String> {   // Ok = {"status": u16, "body": Value}；Err = 通道故障
    state.admin_request(method, path, body).await
        .map(|r| json!({ "status": r.status, "body": r.body }))
}
```

- `lib.rs:278` 注册行 `proxy_admin_token` → `proxy_admin_request`；
- 要点：`body: Option<Value>` 由 Tauri IPC 自动反序列化，前端传对象/null 即可。

**A1.5 J1 测试清单（新增 ~8 例，基线 118 例不退化）**

| # | 用例 | 断言 |
| --- | --- | --- |
| 1 | 进程内 GET overview（hostauth 启用） | 200 + JSON 结构 |
| 2 | 进程内 GET overview（`HostAuth::disabled()`） | 同样 200（InProcess 不依赖令牌开关） |
| 3 | HTTP 面：无令牌 → 401 | 闸门仍在 |
| 4 | HTTP 面：正确令牌 + 回环 peer → 200 | 闸门未误伤 |
| 5 | HTTP 面：正确令牌 + 非回环 peer → 401 | 局域网规则未松（复用 `8ebb74a` 白名单测试基建） |
| 6 | 服务未启动调 admin_request | Err("内置服务未运行") |
| 7 | POST /test 连接测试走进程内 | mock 上游可达（复用 admin.rs chat_test 现有 mock 机制） |
| 8 | stop 后调 admin_request | Err 且不 panic（router 已清空） |

**A1.6 J1 验收**

- `cargo test -p wb-switch-proxy` 全绿（118 + 新增 8 例）；
- `cargo check -p wb-switch-rust` 通过；
- 手工：`wb-proxy-dev --port 8790 --data <tmp>` + curl（带打印的宿主令牌）复跑 `c00b94c` 的实测矩阵——HTTP 排障面完好。

### A2. J2 详细步骤（前端；`02cb5bf` 后前端文件零变动，行号仍准）

**A2.1 `reverse-proxy.ts` 传输层重写（改动集中在 :44-167，约 120 行 → 70 行）**

1. 删：`DEFAULT_BASE`（:16）、`HostProvider` 类型与 `host` 变量、`setHostProvider`、`getBaseUrl`、`setBaseUrl`、`getToken`、`setToken`（:44-94）、`request()` 的 fetch/401 重取/网络错误包装（:116-155）；
2. 增：

```ts
type AdminChannel = (method: string, path: string, body?: unknown)
  => Promise<{ status: number; body: unknown }>;
let channel: AdminChannel | null = null;
export function setAdminChannel(next: AdminChannel | null): void { channel = next; }

async function request<T>(path: string, options: { method?: string; body?: unknown; auth?: boolean } = {}): Promise<T> {
  const call = channel;
  if (!call) throw new RpError("通道未就绪", 0);
  let reply: { status: number; body: unknown };
  try {
    reply = await call(options.method ?? "GET", path, options.body);
  } catch {
    throw new RpError("无法连接反代服务，请确认它已启动", 0);
  }
  if (reply.status >= 400) {
    throw new RpError(extractDetail(reply.body) || `请求失败（HTTP ${reply.status}）`, reply.status);
  }
  return reply.body as T;
}
```

3. `auth` option 保留在类型里（`rp.login*` 等签名兼容）但实现层忽略；`rp.login/loginKey/loginToken/loginOptions/logout/revokeSessions`（:772-785）加 `@deprecated 仅 old 页经 Python HTTP 使用` 注释——实施时先 grep 确认内置页面确无调用点（legacy 页登录表单走自己的 fetch，不经 `rp.*`）；
4. `extractDetail`（:157-167）原样保留；`apiBaseFromService` / `modelsEndpoint`（:1040/:1044）原样保留（数据面地址，与本次正交）。

**A2.2 `ReverseProxyLayout.tsx`（:87-95，~15 行）**

- 删 `setHostProvider({ base, token })` / `setHostProvider(null)` 分支；
- 改为一次装配：`setAdminChannel((method, path, body) => invoke("proxy_admin_request", { method, path, body: body ?? null }))`；
- **语义变化（必须意识到）**：旧形态服务停机 → status.baseUrl 为空 → `setHostProvider(null)` → 请求立刻失败；新形态通道**恒可用**，停机时每个请求都会 invoke 一次再拿 Err。两点补偿：① 检查各 `loadXxx` effect 在 `phase !== "running"` 时**不发起** `rp.*` 调用（避免停机时空打一圈 Err toast）；② 通道 Err 的 toast 文案维持「无法连接反代服务」。

**A2.3 base 地址消费者迁移（3 处）**

| 文件 | 现状 | 改法 |
| --- | --- | --- |
| `cli-link-card.tsx:10` | `getBaseUrl()` 拼数据面地址展示 | props 传入 `proxy_service_status` 的 `baseUrl`（Layout 已持有 status 向下传） |
| `service-control.tsx:7` | old 页用它拼 Python 地址 | 模块内常量 `http://127.0.0.1:8788`，与 `reverse-proxy.ts` 解耦 |
| `reverse-proxy-service.ts:3` | import `DEFAULT_BASE/getBaseUrl/setBaseUrl` | 本地常量；核实 old 页是否仍有「手填地址」（v2 I5 后应只剩 8788），有则把状态收进 old 页内部 |

**A2.4 `proxy-service.ts`（~10 行）**

- `ProxyServiceStatus` 删 `hostToken`（:29-30）；`proxyAdminToken()` 删（:52-60）；`proxy-service.test.ts:25` 桩字段删；`portLine`（:85-91）不动。

**A2.5 `AccountsPage.tsx`（:62、:513-527）**

- 现状：`loadPoolMap()` 用 `getProxyToken()` 判「没登录过就不撞 401」；
- 新形态下令牌概念消失、进程内调用不产生 401；未启动时 `rp.poolSummary()` 走通道 Err → catch → `setPoolMap({})`，行为天然正确；
- 改法：删 `getProxyToken` 判断与 import（:62、:515），直接调 `rp.poolSummary()`；30s 轮询打进程内通道开销可忽略；本页写操作走 `rp.*` 自动随通道。

**A2.6 测试改造 `reverse-proxy.test.ts`（:19-75 骨架）**

- `stubFetch`/`call` 帮手（:32-48）删除；`setHostProvider`（:54）换成：

```ts
const calls: Array<{ method: string; path: string; body?: unknown }> = [];
let stubStatus = 200; let stubBody: unknown = { ok: true };
setAdminChannel(async (method, path, body) => {
  calls.push({ method, path, body });
  return { status: stubStatus, body: stubBody };
});
```

- 断言从 `expect(url).toBe("http://127.0.0.1:8787/admin/api/accounts/a1/actions/active")`（:70）改为 `expect(calls[0].path).toBe("accounts/a1/actions/active")`——`/admin/api/` 前缀从前端契约消失，这是断言层唯一成批变化点（32 例约 20 例受影响，断言方法/字段名的其余用例不动）；
- **桩数据按 `02cb5bf` 之后的最新契约写**（overview.groups 数组、日志键类型等），不照抄旧形状；
- 新增 2 例：`stubStatus = 409` → `RpError.status === 409` 且 message 取 `detail`；channel reject → `status === 0`；
- `stubLocalStorage`（:21-30）视 `getSessionUser`/`setSessionUser` 是否仍被 legacy 页引用决定去留。

**A2.7 J2 验收**

- DevTools Network：`/reverse-proxy/*` 全站操作，对 `/admin/api/*` 的 fetch **0 条**（顺带：CORS 预检 OPTIONS 也应为 0 条——`a861be5` 那类故障被根除的可见标志）；
- 21 个面板逐页操作与改造前一致；
- 停机 → 打开反代页 → 「已停止 + 启动」→ 启动后恢复，全程无刷新；
- `npm test` / `npx tsc --noEmit` 全绿。

### A3. J3 详细步骤（~0.5 天）

1. 残留核查：`grep -rn "proxyAdminToken|proxy_admin_token|setHostProvider|getBaseUrl|hostToken|DEFAULT_BASE" src/ src-tauri/` 预期 0 命中（`proxy_service.rs` 内部 `host_token()` 与 `hostauth.rs` 的令牌逻辑除外——服务端闸门，有意保留）；
2. 全站再查一遍 `rp.login*` 引用（A2.1 第 3 点兜底确认）；
3. 注释与文档：`proxy_service.rs` 模块注释、`hostauth.rs` doc（「前端经 invoke 取令牌放请求头」已非事实）、`http.rs:4`、crate `lib.rs:7`；`docs/DEVELOPMENT.md`、`CODEBUDDY.md` 补「管理面调用约定：新增管理端点必须 JSON-in/JSON-out；前端一律经 `rp.*`，禁止绕过通道」；排障指引写入 `wb-proxy-dev` 用法（`c00b94c` 顶部注释已完整，链接过去）；
4. v2 文档（`docs/reverse-proxy-integration-plan.md`）顶部加一行指向本文（同 v2 → v1 先例）。

### A4. 提交切分（每步独立编译/测试通过）

| commit | 内容 | 验证 |
| --- | --- | --- |
| 1 | A1.1（InProcess + authorize 三分支）+ A1.5 #3/#4/#5 | `cargo test -p wb-switch-proxy`（118 + 3 新例） |
| 2 | A1.2（router 构造上提，纯重构，断言零变化） | 同上，118 例不退化 |
| 3 | A1.3 + A1.4 + A1.5 其余（通道 + 命令 + 单测） | `cargo check -p wb-switch-rust` + 手工 curl 矩阵 |
| 4 | **A2.1 + A2.2 + A2.4（必须同一 commit）**：传输层换引擎 + 通道装配 + 状态裁剪 | `tsc --noEmit` + `npm test` |
| 5 | A2.3 + A2.5（base 迁移 + AccountsPage 清理） | `npm test` + DevTools 手工核查 |
| 6 | A2.6 + A3（测试断言迁移 + 文档） | 全量 CI |

**为什么 commit 4 必须合并**：A2.2 装配通道但 A2.1 未换引擎 → 通道是死代码；反过来 request 走 invoke 但没人装配 → 全部请求失败。两者是同一次语义切换的两半，拆开必产生不可用中间态。

**回滚策略**：commit 1-3 纯后端（HTTP 面行为不变，页面仍走 fetch），可单独回滚；commit 4 之后前端已切换，回滚需 4、5 一起回。无数据迁移、无 schema 变化，回滚无状态残留（唯一持久化变更 `settings.reverse_proxy.port` 来自 v2，与本文无关）。

---

## 附录 C. 决策追加（2026-10-30，用户拍板）：HTTP 管理面整体移除；IP 安全重新定位为「数据面（智能体 API）专属」

> 本节**取代**正文 §3.6（「HTTP 管理面保留为排障面」）与附录 B 中与之冲突的表述。附录 B 的移除决策（tokens/users/audit）不变且被本节覆盖吸收——B 节的 13 条路由现在是「摘除管理面 HTTP」这个更大动作的子集。

### C.0 决策内容

1. **反代服务对外只保留数据面 HTTP**：`/health` + `/v1/*`（智能体 API 协议接口）。`/admin/api/*` 从 HTTP router 上**整体摘除**——进程内通道落地后，管理面不再有任何 HTTP 消费者（附录 B 已论证排障面也不成立：宿主令牌打印能力由 `proxy_admin_request` 通道与桌面端 UI 取代）。
2. **IP 安全（security）重新定位**：它的作用对象是**数据面 API 密钥**（`keys.rs` 的 `ip_allowed`，按 key 校验请求来源），从来不是管理面的 HTTP 安全。管理面 HTTP 消失后，「管理面鉴权」这一层（hostauth / ADMIN_KEY / 会话）失去存在意义，而 IP 白名单作为数据面能力**完整保留**，UI 恢复展示（v2 §3.5 的「隐藏」随本节撤销）。

### C.1 被本节取代的既有表述（修正索引）

| 位置 | 原表述 | 修正 |
| --- | --- | --- |
| 正文 §3.6 | HTTP 管理面「不删」，降级为排障面 | **整体摘除**；`wb-proxy-dev` 改为挂**纯数据面** router（见 C.3） |
| 正文 §3.1 要点 4 / J1 | 「hostauth 保留在线上」「HTTP 面带令牌仍 200」 | hostauth 的**令牌比对逻辑随管理面 HTTP 一起退役**；`HostAuth` 类型收窄为「进程内旁路标记的载体」（C.2） |
| 正文 §5 测试策略 / A1.5 #3-#5 | 「带令牌 200 / 不带 401 / 非回环拒绝」四象限 | 改为**负向测试**：`/admin/api/*` 任何路径经 HTTP 返回 404 |
| 正文 §7 验收 | 「HTTP 管理面闸门未松：带令牌 200…」 | 改为「HTTP 上不存在管理面：`curl /admin/api/overview` → 404」 |
| 附录 A3 第 1 条 | grep 保留「hostauth 令牌逻辑（服务端闸门，有意保留）」 | hostauth 的 token/sessions 逻辑删除；仅 `InProcess` 标记与 `peer_from_connect`（数据面白名单用）保留 |
| 附录 B B.1 第 2 条 | `AdminState` 结构保留 | **随管理面 HTTP 一起移除**（B 节保留它的理由是「测试在用」，测试也删了）；`admin.rs` 的 router 与 handler 同步瘦身 |

附录 B 其余内容（tokens/users/audit 移除、state.json 三键保留、old 页面板保留、`rp.login*` 删除）**不变**，且 B.2 的「commit 1 摘 13 条路由」扩大为「commit 1 摘管理面 HTTP 全部路由」。

### C.2 技术要点

1. **`http.rs::router()` 摘除 `.nest("/admin/api", ...)`**（`http.rs:62`）。数据面 router 变成纯数据面：`/health`、`/v1/models`、4 条对话接口、`/v1/usage`。CORS 层（`a861be5`）**随之失去意义，一并删除**——CORS 只为浏览器跨源打管理面而生，数据面的消费者是 CLI/SDK（非浏览器），不需要预检。
2. **`hostauth.rs` 瘦身**：删 `HostAuth` 结构、`authorize`、`HOST_TOKEN_HEADER`、`from_env`、`random_token`、`constant_time_eq`；保留 `peer_from_connect` / `peer_ip`（数据面白名单的 ConnectInfo 拆包，`8ebb74a` 引入）并新增 `InProcess` 标记。模块改名建议：`hostauth.rs` → `peer.rs`（它现在只管「来源识别」，与令牌无关）——改名可选，实施时顺手做。
3. **`ProxyService`**：不再生成/持有宿主令牌（`host_token()`、`HostAuth::with_token` 调用点删除）；`start_with_auth` 变体与 `AppState.host_auth` 字段删除；A1.1 的 `authorize` 三分支改造**取消**——进程内通道直接打 admin 子 router，鉴权中间件不复存在，`InProcess` 也**不再需要**（admin router 摘出 HTTP 后没有第二个入口，进程内是唯一调用方，天然可信）。
4. **`admin.rs` 大幅瘦身**：`AppState` 删 `admin: Arc<AdminState>` 与 `host_auth` 字段；`session_of` / `authorize` / `login` / `logout` / `session_info` / `revoke_sessions` / `login_options` / `unauthorized`-会话相关 helper、`users` / `tokens` / `audit-logs` 全部 handler 与 13+ 条路由删除。admin router 剩余：overview、pool、keys、groups、egress、models、aliases、request-logs、usage-stats、settings、alerts、cli-key、test（约 40 条路径）。
5. **`AppState` 上提（A1.2）照旧**，且更简单：不再需要「完整 router 与 admin 子 router 共享同一份 AppState 的微妙性」——admin 子 router 就是唯一形态。
6. **`wb-proxy-dev`**（`c00b94c`）：改为只挂数据面 router；它的宿主令牌打印与 `--port/--data` 参数保留（宿主令牌概念随管理面消失后，改为打印**不需要**——它本来就只 serve 数据面，删令牌相关代码即可）。curl 实测矩阵中「宿主令牌三例」作废，白名单/密钥 CRUD/usage/连接测试/端口偏移各例保留。
7. **安全审计视角的净效果**（写进发布说明）：桌面端单进程在回环上只暴露智能体 API；管理能力零网络暴露（只经 Tauri IPC）；同机其它进程能打的面从「数据面 + 管理 HTTP」收窄到「数据面」。

### C.3 实施归属（覆盖 A4 的 commit 切分）

| commit | 内容（相对 A4 的变化） |
| --- | --- |
| 1 | A1.1 **改写**：不做 authorize 三分支（取消）；直接摘除 admin HTTP——`http.rs` 去 nest、去 CORS；`admin.rs` 删 session/users/tokens/audit 全部路由与 handler（B 节 13 条 + login/session 6 条 + hostauth 依赖）；`hostauth` 瘦身为 peer 识别 + InProcess 保留与否按 C.2.3 定（不需要则不保留）；`wb-proxy-dev` 改纯数据面。测试：admin 相关用例重写为「HTTP 404」负向断言 + 数据面白名单用例（118 例中 admin 会话类用例删除） |
| 2 | A1.2 不变（build_state 上提，字段少了 admin/host_auth 更简单） |
| 3 | A1.3 **改写**：`admin_request` 通道打 admin 子 router，无 InProcess extension、无令牌组装；`status()` 删 `hostToken` |
| 4-6 | A2.x 不变（前端改动与 B 节归属完全一致）；A3 文档步骤中「hostauth doc 改写」改为「hostauth/peer doc 更新 + 发布说明按 C.2.7」 |

**回滚不变**：commit 1 现在是行为删除（HTTP 管理面消失），回滚即恢复 nest——无状态迁移。

### C.4 验收增量（并入 §7）

- [ ] `curl 127.0.0.1:8787/admin/api/overview` → **404**（管理面在 HTTP 上不存在）；
- [ ] `curl 127.0.0.1:8787/health` → 200；数据面四条接口带有效 API key 正常（`8ebb74a` 白名单矩阵复跑）；
- [ ] 代码中不存在 `X-Proxy-Host-Token` / `ADMIN_KEY` / 会话 cookie（`grep -rn "HOST_TOKEN_HEADER\|ADMIN_KEY\|sessions" crates/wb-switch-proxy/src/` 仅剩 state.json 键名与 old 相关注释）；
- [ ] DevTools Network：管理面 HTTP 请求 0 条、CORS 预检 0 条；
- [ ] IP 安全页签恢复展示（v2 §3.5 的隐藏撤销），配置读写走进程内通道，白名单对数据面生效（`c00b94c` 矩阵）；
- [ ] `wb-proxy-dev` 可独立起数据面并跑通白名单/密钥/usage/连接测试矩阵。

---

## 附录 D. 决策追加（2026-10-30，用户拍板）：内置服务恢复「对局域网开放」；移除「服务名称」

### D.0 背景（核对代码后的结论）

**局域网开放不是 Rust 没做，是 UI 入口没跟上。** 能力链路完整：`ProxyConfig.host` 支持 `WB_SERVE_HOST` 环境变量（`config.rs:36`，默认 `127.0.0.1`）、`bind` 接受任意 host、数据面白名单（`8ebb74a`）能约束局域网来源。缺的是 v2 §3.7 承诺的「局域网开关落到服务设置 → 内置服务」——I1-I6 实施时未落地，且 `ProxyService::start()` 从不读用户设置里的 host，`status()` 把 baseUrl 硬编码为 `127.0.0.1:{port}`（`proxy_service.rs:89`）。Python 的开关在 `service-control.tsx`（「对局域网开放」按钮 → 以 0.0.0.0 重启），只服务 old 页。

**服务名称（`site_name`）确认可删**：它是 Python 网页版管理后台的 `<title>` 字段（`service-settings-panel.tsx:96-108` 自述「显示在管理页面标题处」）。Rust 形态没有任何 UI 消费它；`/admin/api/settings` 的读写端点（`admin.rs:191,398,1683`）在附录 C 摘除管理面 HTTP 后，其消费方只剩本面板这一个字段。删除后 `service-settings-panel` 的「基础」分区变空，正好按 v2 §3.7 的原计划重组为「内置服务卡片」的宿主。

### D.1 局域网开放（内置服务）

**语义**（沿用 `service-control.tsx` 的既有决定，`reverse-proxy-service.ts:51-66` 注释）：

- 只对**数据面**生效——附录 C 之后管理面在 HTTP 上不存在，局域网暴露的只有 `/health` + `/v1/*`，鉴权靠 API key + 密钥级 IP 白名单（`keys.rs ip_allowed`）。**局域网开放 + 白名单为空的密钥 = 该密钥对全网段可用**，UI 要把这两件事联动提示。
- 「每次启动重新确认，不写进配置」改为**写进配置**（`settings.reverse_proxy.host`，与 port 同位）——旧语义是 Python 托管形态的妥协（每次重启都要问一遍），内置服务随主程序启动、没有重启问答窗口，必须持久化。默认 `127.0.0.1`。
- 期望端口冲突 +1 逻辑不变（v2 §3.1.1）；host 不参与偏移（bind 失败即 Failed，不做 0.0.0.0 → 127.0.0.1 的静默降级——暴露面收窄必须显式）。

**实施明细**：

| 层 | 改动 |
| --- | --- |
| `config.rs` | `for_user()` 读 host 的顺序增加 state.json：`WB_SERVE_HOST` → `settings.reverse_proxy.host` → `127.0.0.1`（与 port 同构） |
| `proxy_service.rs` | `start()` 读取并透传 host（现有 `cfg = ProxyConfig::for_user()` 已覆盖，只需在 `saved_port` 旁加 `saved_host`）；`status()` 的 baseUrl 改为 `http://{host}:{port}`（:89 去硬编码），新增 `host` / `lanExposed` 字段；`set_host(host)` 命令（停机 → 改配置 → 启动，与 `set_port` 同形；合并为 `set_bind(host, port)` 更好——两个 setter 各自停启两次是浪费，一次重绑到位） |
| `hostauth.rs`/`keys.rs` | 不动——白名单已按 `peer` 判定，0.0.0.0 下局域网请求来源非回环，靠白名单放行/拒绝 |
| 前端 `service-settings-panel.tsx` | 重组为「内置服务」卡片（v2 F8 的原计划落地）：监听范围（本机 / 局域网）开关 + 期望端口 + 数据目录 + 启停；局域网开启时黄条提示「请为 API 密钥配置 IP 白名单，否则全网段可用」并链接到 API 密钥页；baseUrl 展示区分 `127.0.0.1`（本机）与 `0.0.0.0`（列出本机各网卡 IP 供复制，或提示「使用本机局域网 IP + 实际端口」——列网卡 IP 需要后端枚举，可后端在 status 里给 `lanIps: string[]`，实施时按成本决定先做提示版） |
| 前端 `proxy-service.ts` | `ProxyServiceStatus` 加 `host` / `lanExposed`；`proxyServiceSetBind` |
| `site_name` 清理 | `RpServiceSettings`/`RpSettingsPatch` 删 `site_name`（`reverse-proxy.ts:532,538`）；`reverse-proxy.test.ts:291-309` 两例删；**old 页不受影响**（它读 Python 的 settings，`service-settings-panel` 不被 old 引用——需在实施时 grep 确认 `site_name` 无 legacy 引用） |

### D.2 归属

- Rust 侧（config + proxy_service + set_bind 命令 + 测试）：**commit 3**（A1.3 通道同阶段，都在 `proxy_service.rs`）；
- 前端（内置服务卡片重组 + site_name 清理）：**commit 5**（A2.3 同阶段）；
- 验收增量：开启局域网 → `status.lanExposed=true`、状态条显示 0.0.0.0；局域网另一台设备 `curl http://<内网IP>:8787/v1/models`（带白名单内 IP 的密钥）→ 200；白名单外 → 403；关掉 → 恢复本机。

### D.3 对既有章节的增量修正

- v2 §3.7 的「局域网开关……语义仍是『每次启动重新确认，不写进配置』」由本附录 D.1 取代（持久化）。
- 附录 C.4 验收补一条：局域网开放时管理面仍不存在（`curl http://<内网IP>:8787/admin/api/overview` → 404）。
- 「服务设置」页签在 nav 中保留，内容重组为内置服务卡片（不再有「基础/site_name」）。

---

## 附录 B. 决策追加（2026-10-30，用户拍板）：移除「管理令牌 / 后台账号 / 审计」

> 背景：进程内通道落地后，HTTP 管理面只剩排障用途。v2 §3.5 曾以「HTTP 面仍是完整契约」为由保留这三个页签的后端；现在这条理由不再成立，用户拍板**整体移除**。

### B.0 移除依据（逐项核对代码）

| 功能 | 死因 | 代码事实 |
| --- | --- | --- |
| 管理令牌（wbt_…） | 令牌的唯一用途是「外部 HTTP 客户端换会话」。进程内调用不经会话；排障走 `wb-proxy-dev` 打印的宿主令牌。**没有消费者了** | `admin.rs` 的 `/tokens` CRUD 4 条路由（:175-176）；前端 `tokens-panel.tsx`、`rp.listTokens` 等 4 个方法（`reverse-proxy.ts:835-840`）、`RpToken` 类型、`nav.ts:60` |
| 后台账号（多用户 PBKDF2） | 管理面入口只剩「宿主令牌 + 回环」，登录/会话/多角色整个体系闲置（v2 §3.5 已隐藏，现在连后端一起收） | `admin.rs` 的 `/users` CRUD 3 条路由（:173-174）、`/login*` / `/session` / `/logout` / `/sessions/revoke` / `/login-options` 6 条（:147-152）、`Session`/`sessions` 机制；前端 `users-panel.tsx`、`rp.listUsers` 等 3 个方法（:842-847）、`nav.ts:61` |
| 审计（audit-logs） | **Rust 从不写审计**：`admin.rs` 只有读（:410 `audit_logs`）与清（:1478 `clear_audit_logs`），没有任何 push 写入点——数据只能来自 Python。内置服务形态下这条链路天生是空的 | `admin.rs` 2 条路由（:160、:182）；`state.rs:15,31,44` 的 `audit_logs` 键（**见 B.2 例外，不能删**）；前端 `AuditPage.tsx`、`audit-panel.tsx`、`rp.auditLogs`/`clearAuditLogs`（:857-858）、`RpAuditLog` 类型、`nav.ts:63`、**`security-panel.tsx` 内嵌的「审计日志」子 Tab（:187-223）** |

连带收敛（同一理由）：

- `security-panel.tsx` 里的「审计日志」子 Tab 一并删除（它读的就是同一份死数据），IP 安全面板只剩配置 + IP 访问日志两个 Tab；
- `rp.login / loginKey / loginToken / loginOptions / session / logout / revokeSessions`（:772-785）改为随 A2.1 **直接删除**而非 `@deprecated`——session 机制退役后它们连 old 页都不会再调（old 用的是 Python 自己的 `/admin/api/*`，经 `reverse-proxy-service.ts` 的 fetch，不经 `rp.*`）。

### B.1 不删的东西（边界）

1. **`state.rs` 的 `users` / `api_tokens` / `audit_logs` 三个键（:15、:31、:44）**——`state.json` 与 Python old **共用**（v2 D4），Python 还在读写这三个键；删默认结构会让 old 读到残缺 state。键保留，只删 Rust 的读写端点。
2. **`AdminState` 结构体本身**（`admin.rs`，`key` / `sessions` 字段）——`authorize` 的第三分支（session 回退）虽然不再有 HTTP 消费者，但它是 `/login` 判定与 Python 契约对齐的载体；`AdminState::from_env()` 与 `with_admin()` 在测试里仍在用。**结构保留，`/users` `/tokens` `/login*` 等 13 条路由摘除**。
3. **`RpSession` 类型（`reverse-proxy.ts:210`）**——`users-panel` 删掉后若无引用则随之删除；实施时以编译器为准。
4. **old 页（`LegacyReverseProxyPage`）里 import 的 `AuditPanel` / `TokensPanel` / `UsersPanel`**（`legacy/ReverseProxyPage.tsx:24,34,35`）——old 页**功能原样保留**（v2 §3.4），这三个面板组件文件**不删**，只是从内置页面菜单摘除。组件归属不变：它们继续服务 old。

### B.2 实施归属（并入既有阶段，不新增 commit）

| 动作 | 归属 | 明细 |
| --- | --- | --- |
| Rust：摘 13 条路由（`/login` `/logout` `/session` `/me` `/sessions/revoke` `/login-options` `/users`×3 `/tokens`×4 `/audit-logs`×2）+ 对应 handler | **commit 1**（A1.1 同步做） | 摘路由与改造 `authorize` 在同一个函数群，一次改完省一轮测试往返；`admin.rs:145-201` 的路由表与 :203-420、:1478-1490 的 handler。**注意**：`login_options`（:203）返回值引用的 `legacy_key` 字段随之消失，无消费方，直接删 |
| Rust：`AuditLog` 相关测试清理 | commit 1 | `tests/admin.rs` 里 login/session/users/tokens/audit 的用例随端点删除；hostauth 用例（A1.5 #3-#5）不受影响 |
| 前端：删 `tokens/users` 方法与类型、`auditLogs`/`clearAuditLogs`、`RpToken`/`RpUser`/`RpAuditLog`、`login*` 7 个方法 | commit 4（A2.1 同步做） | `reverse-proxy.ts`；`nav.ts` 的 `count` 类型里 `"tokens" \| "users"` 两个枚举值删除（:16），`nav.ts:60-63` 高级组只剩 `security` |
| 前端：删 `TokensPage`/`UsersPage`/`AuditPage` 薄壳 + `App.tsx:408` 等三条路由 | commit 4 | `src/pages/reverse-proxy/{TokensPage,UsersPage,AuditPage}.tsx`；`ReverseProxyLayout` 的计数徽标来源同步收敛 |
| 前端：`security-panel.tsx` 删审计子 Tab（:187-223）与 `rp.auditLogs` 依赖（:27 的 Promise.all 改三项、:18 的 state） | commit 4 | |
| 测试：`rp-013.test.ts` 的 audit 用例（:119-134）删 | commit 6（A2.6 同步做） | |

**B.2.1 面板组件文件的归属修正**：`tokens-panel.tsx` / `users-panel.tsx` / `audit-panel.tsx` **不删**（old 页仍引用，B.1 第 4 条）；但内置页对它们的唯一引用（薄壳 Page + 路由）删除后，若 `tsc` 报出它们 import 的 `rp.listTokens` 等方法不存在——**这正是 commit 4 的一致性验收点**：三个面板改为从 props 接收数据或直接在面板内 fetch old 地址？**都不做。正确做法：这三个面板 import 的 `rp.*` 方法保留为「仅 old 使用」的窄接口**（不移除 `listTokens/listUsers/auditLogs` 等 10 个方法本身，只删内置页的调用与菜单入口）。修正 B.2 第三行的范围：删的是**内置页对它们的调用**（薄壳 Page、nav 项、计数），`rp.*` 方法与类型保留并标注 `@deprecated 仅 old 页使用`。

### B.3 验收增量

- 内置控制台「高级」分组只剩「IP 安全」一项（且其内部无审计 Tab）；old 页三个面板功能不变；
- `cargo test`：admin 路由 13 条摘除后 118 例中对应用例删除，其余全绿；`grep -rn "audit_logs" crates/wb-switch-proxy/src/admin.rs` 0 命中；
- `grep -rn "listTokens\|listUsers\|auditLogs" src/` 命中仅剩 old 页面板与 `rp.*` 定义本身；
- v2 §3.5 的「隐藏待议」三条（tokens/users/security 表格）与 v2 §10 遗留清单，由本文 B 节取代。
