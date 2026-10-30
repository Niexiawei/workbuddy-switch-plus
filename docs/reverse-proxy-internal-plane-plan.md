# 反代融合计划（v3）：管理面去 HTTP 化 —— 页面与反代同进程直连，对外只留智能体数据面

> 状态：**方案稿**，本文档只写计划，不含任何代码改动。
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

## 4. 分阶段实施（每阶段独立可验收）

### J1 Rust：进程内管理面通道（约 0.5 天）

- `src-tauri/src/proxy_service.rs`：
  - `Inner` 增加 `admin_router: Option<Arc<axum::Router>>`；`start()` 构建、`stop()` 清空；
  - 新增 `admin_request(method, path, body)`（§3.1 形状，含 `PeerLoopback` extension、8MB 上限、锁外 await）；
  - `status()` 移除 `hostToken` 字段。
- `crates/wb-switch-proxy/src/hostauth.rs`：新增 `PeerLoopback` 类型与识别逻辑；`authorize` 保持不变；doc 注释改写（§3.6）。
- `src-tauri/src/lib.rs`：注册 `proxy_admin_request` 命令；删除 `proxy_admin_token`。

**验收**：
- `cargo test -p wb-switch-proxy` 全绿（现有 45 例不退化）；
- 新增单测：进程内 `admin_request("GET", "overview")` 返回 200 + 正确 JSON；服务未启动时 Err；`POST /test`（连接测试）经进程内通道可走通 mock 上游；
- 新增集成测试：**带 hostauth 头的真回环 HTTP 仍 200、不带仍 401**（证明 HTTP 闸门没有被进程内旁路意外拆掉）。

### J2 前端：传输层换引擎（约 0.5 天）

- `src/lib/reverse-proxy.ts`：按 §3.3 重写 `request()` 与通道抽象；删 `HostProvider` 系、`getBaseUrl`/`setBaseUrl`/`DEFAULT_BASE`、401 重试、`login*` 保留注释「仅 old」；
- `ReverseProxyLayout.tsx`：`setHostProvider` → `setAdminChannel`；通道 Err 且 `phase==="stopped"` 时渲染现有「内置服务已停止 + 启动」卡片（逻辑已存在，只换数据源）；
- `cli-link-card.tsx` / `service-control.tsx` / `reverse-proxy-service.ts`：base 来源改为 §3.3 所列；
- `proxy-service.ts`：删 `proxyAdminToken`、`hostToken` 字段；
- 旧 localStorage 键（`wb.rp.user` 等）按 v2 §3.3 的清理策略复查一遍。

**验收**：
- DevTools Network 面板：进入 `/reverse-proxy/*` 所有页面、执行所有面板操作，**对 `127.0.0.1:<port>/admin/api/*` 的 fetch 为 0 条**；
- 21 个面板逐页操作比对（v2 I2 同款清单）：行为与改造前一致；
- 停机状态下打开反代页：显示「已停止 + 启动」，点击启动后页面自动恢复，**全程无页面刷新**；
- `reverse-proxy.test.ts`（32 例）改造：provider 替换点换成 `setAdminChannel` 固定桩，断言「打到哪个 path、什么 method」的用例全部不变地通过；新增「status 4xx → RpError.status 正确」「通道 Err → status 0」两例；`npm test` / `npx tsc --noEmit` 全绿。

### J3 残留清理与文档（约 0.5 天）

- `AccountsPage.tsx:62` 的 `getProxyToken` 用途核查与处置（§3.4）；
- `src-tauri/src/proxy_service.rs` 顶部模块注释、`hostauth.rs` doc、`CODEBUDDY.md` 反代章节、`docs/DEVELOPMENT.md`：按 §3.6 的「HTTP = 排障面、UI = 进程内通道」改写；
- v2 文档顶部加一行指向本文（同 v2 → v1 的先例）。

**验收**：`grep -r "proxy_admin_token\|setHostProvider\|X-Proxy-Host-Token" src/` 仅剩 hostauth 相关注释；`npm run tauri build` 通过。

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

## 7. 验收标准（全部满足才算完成）

- [ ] DevTools Network：反代控制台全站操作，`/admin/api/*` 的 HTTP 请求为 0 条。
- [ ] 服务停机时反代页面可用（能启动、能看错误原因），不再出现「服务挂了打不开设置页」死循环。
- [ ] 端口偏移（+1 / 20 次耗尽 `Failed`）时控制台功能完整，仅数据面地址按 v2 §3.1.1 提示。
- [ ] 外部客户端零感知：Codex / Claude Code 配 8787（或偏移后端口）照常工作；`/health`、`/v1/*` 行为与改造前一致。
- [ ] HTTP 管理面闸门未松：带令牌 200、不带 401、非回环拒绝（自动化测试覆盖）。
- [ ] `cargo test` / `npm test` / `npx tsc --noEmit` / `npm run tauri build` 全绿。
- [ ] `proxy_admin_token` 命令、`hostToken` 字段、前端 401 重取逻辑在代码中不存在；文档已按「HTTP = 排障面」改写。
