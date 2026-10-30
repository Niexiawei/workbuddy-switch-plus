# 反代服务并轨计划（v2）：独立进程 → 主项目内嵌，webui 内嵌页 → 主项目原生页面

> 状态：**已完成并部分取代**。本文的 I0–I6 全部落地（commit `ee0b46c`…`bf0061c`）。
> 后续演进见 `docs/reverse-proxy-internal-plane-plan.md`（v3）：管理面已去 HTTP 化
> （进程内通道）、tokens/users/audit 功能移除、HTTP 管理面整体摘除、局域网开放——
> v2 §3.3（宿主令牌注入）、§3.5（高级页签取舍）、§3.6/3.6.1 中的部分表述已被 v3 取代。
> 承接：`docs/reverse-proxy-rust-migration-plan.md`（下称 v1，P0–P5 已完成、P6 进行中）。
> 本文取代 v1 §3.2 / §3.3 / §3.4 / §4(D6) / §6(P6、P7) 的形态约定；v1 的协议迁移结论（P0–P5）继续有效。
> 约定：引用 v1 时写「v1 §X」，引用本文件时写「本文 §X」，两者小节号会重号。

---

## 0. 本次要解决的四件事

用一句话概括：**Rust 反代已经写好了，但它还停在「可选引擎 + 需要手动启动 + 用 Python 那套内嵌页面」的半成品状态。**

| # | 现状 | 目标 |
| --- | --- | --- |
| 1 | Rust 反代由 `src-tauri/src/proxy_engine.rs` 托管，**要用户点「启动」才监听**；默认还是 Python 引擎（`getEngine()` 无值时返回 `python`） | Rust 反代合并进主项目，**随主程序启动自动监听**，生命周期与主进程同生共死 |
| 2 | 管理面客户端 `src/lib/reverse-proxy.ts` 打的是 `http://127.0.0.1:8787/admin/api/*`，而 **8787 是 Python 的约定端口**；Rust 引擎顶在 8788（灰度端口） | 主项目原生页面直连**进程内**服务，端口由后端给出（可自定义、冲突自动 +1，见本文 §3.1.1），前端不再猜 |
| 3 | 界面沿用主项目 `src/components/reverse-proxy/*`（v1 早期已做），但**登录/鉴权那一段仍是 Python 那套**（账号口令 / 管理密钥 / 管理令牌 + 保存口令到磁盘 + 记住我），路由也只有 `/reverse-proxy` 一个平铺页签 | **主项目原生 React/TSX 页面**：控制台不再有「登录页」，改为主程序身份；现有页签作为**二级菜单挂在「反代服务」下** |
| 4 | Python 版本保留，但入口、端口、数据目录与 Rust 完全混在同一个 `/reverse-proxy` 页里（靠顶部「Python / Rust」切换器） | Python 版本作为独立入口 **「反代服务old」**（路由 `/reverse-proxy-old`、端口 8788）；数据目录仍共用，靠「只启用一个」互斥（见本文 §5 D4） |

> 关于「webui 内嵌页面」：Python 反代自带一套静态后台 `reverse-proxy/admin/static/*`（`index.html` 57 KB + `app.js` 145 KB + `style.css` 65 KB，见 `reverse-proxy/admin/static/`），由 `admin/server.py` 挂在 `/admin/` 下。主项目里（`src/components/reverse-proxy/*`，21 个文件、约 6.2k 行）其实已经有一套 React 版管理界面，v1 §3.2 就是照它写的 —— 但**登录/会话、页面骨架、Tab 组织仍是 Python 后台的形状**，并且 Rust 引擎下不少控件点了会 501。本文的「原生页面」= 把这套 React 页面按主项目的约定重排一遍，而不是从零再写一遍。
>
> 已拍板的七项口径见本文 §10（端口自定义 + 冲突 +1、共用数据目录、连接测试保留 / IP 安全延后、不做 BFF、补 `/v1/usage`、移除 `wb-switch`）。

---

## 1. 现状盘点（对照代码）

### 1.1 Rust 反代本体：`crates/wb-switch-proxy`（`src/` 约 6.2k 行，`tests/` 约 0.7k 行，45 个测试）

| 模块 | 文件 | 状态 |
| --- | --- | --- |
| 句柄 / 启停 | `src/lib.rs`（`ProxyHandle::start/stop/status`） | ✅ 可被任何宿主内嵌，**目前只有 `src-tauri/src/proxy_engine.rs` 在用** |
| 路径与端口 | `src/config.rs`（`for_user()` 读 `WB_SERVE_HOST` / `WB_SERVE_PORT`，默认 `127.0.0.1:8787`，数据目录 `~/.wb-switch-plus/reverse-proxy`） | ✅ |
| 状态 | `src/state.rs`（27 键、只读/可写、原子写） | ✅ |
| 数据面 | `http.rs` + `chat.rs` / `responses.rs` / `anthropic.rs` / `wire.rs` / `projection.rs` / `desensitize.rs` / `prompt.rs` / `profile.rs` / `errors.rs` / `catalog.rs` | ✅ `/health`、`/v1/models`、4 条对话接口（`/v1/usage` 未接，见本文 §10 第 5 条） |
| 池与密钥 | `pool.rs` / `keys.rs` / `billing.rs` | ✅ 余 IP 白名单与用量累加（v1 P4「刻意未做」） |
| 管理面 | `admin.rs`（1521 行，54 条路径 / 70 个方法）+ `/admin/api/*` 挂载在 `http.rs:49` | ⚠️ 路由齐全，但鉴权只有 **`ADMIN_KEY` 单密钥** + 内存会话；有 13 处 501 / 占位实现（见 §3.5） |
| 独立入口 | `src/bin/wb-proxy.rs` | 🔴 **正是要消灭的「独立进程」入口** |
| 测试 | `tests/{skeleton,chat,catalog,admin}.rs` + 模块内单测 | 45 个用例 |

### 1.2 宿主

- **桌面端（Tauri）**：`src-tauri/src/proxy_engine.rs` 用 `Mutex<Option<ProxyHandle>>` 托管，暴露 `proxy_engine_status` / `proxy_engine_start` / `proxy_engine_stop` 三个命令（注册在 `src-tauri/src/lib.rs` 的 `invoke_handler` 里），`lib.rs` 的 `setup` **没有**调用它。同时 `src-tauri/src/reverse_proxy.rs`（846 行）仍在管 Python exe 的「找 exe / 起进程 / 探测端口 / 提权停止」。
- **webui（`crates/wb-switch-server`）**：`main.rs` 起 axum（默认 57890），`api.rs` 有 48 条 `/api/*` 路由 + 用 `rust-embed` 把 `dist/` 打进二进制做静态站（`api.rs:52`）。**完全没有反代路由**，前端在浏览器里直连 `127.0.0.1:8787` 再自己带 `X-Admin-Token`（v1 §3.4 / P6「webui BFF」未做）。→ **决策 6：整包移除**，见本文 §3.6。

### 1.3 前端

| 位置 | 角色 | 本次命运 |
| --- | --- | --- |
| `src/pages/ReverseProxyPage.tsx`（约 557 行） | 一个页面装下：登录门（`ConnectView` / `LoginCard`）、引擎切换、常驻状态条、竖排 16 个 Tab、添加账号 / 批量扫码弹窗 | 拆分（见 §3.2） |
| `src/components/reverse-proxy/*`（21 个文件） | 16 个面板 + `service-control.tsx`（482 行，Python 进程控制台）+ `rp-shared.tsx` | 面板保留改造；`service-control.tsx` 降级（见 §3.4 / §4 F7） |
| `src/lib/reverse-proxy.ts`（993 行） | 管理面 HTTP 客户端，`DEFAULT_BASE = "http://127.0.0.1:8787"`，base/token 存 localStorage | 改 base 来源 + 去掉令牌流程 |
| `src/lib/proxy-engine.ts` | 引擎选择（localStorage `wb.rp.engine`）、Rust 固定 8788、`proxy_engine_*` 调用 | 删除（由新客户端接管） |
| `src/lib/reverse-proxy-service.ts` | Python 进程管控（status/start/stop/提权/换端口/保存登录） | 只留给「反代服务old」 |
| `src/components/reverse-proxy/service-settings-panel.tsx` | 服务设置面板 | 需要新增「内置服务（停机/端口）」分区，承接被砍掉的 `ServiceControl` 能力 |
| `src/pages/SettingsPage.tsx` | 引用了 `serviceCloseOnExitEnabled` / `setServiceCloseOnExit`（`src/pages/SettingsPage.tsx:69`） | 开关语义要跟着改（见本文 §3.3） |
| 测试 | `src/lib/*.test.ts` 共 160 个用例，其中反代相关 4 个文件 76 个（`reverse-proxy` 32 / `rp-limits` 13 / `rp-013` 15 / `reverse-proxy-panels` 16）；vitest 现为 `environment: "node"`、只收 `*.test.ts` | 新增判定逻辑要有纯函数可测（见 §7） |

### 1.4 为什么「现在这个形态」必须结束

- **端口对不上**：Rust 的 `ProxyConfig` 默认就是 8787，前端为了双跑把 Rust 顶到 8788（`proxy-engine.ts` 的 `RUST_ENGINE_PORT`）。一旦 Rust 成为唯一引擎，这个 8788 就是个不必要的偏差：外部客户端（Codex / Claude Code）文档里写的都是 8787。
- **生命周期还挂在用户手上**：`ProxyEngine::start` 只在用户点按钮时被调用，进程退出也没有人调 `stop`（`lib.rs` 的 `RunEvent::Exit` 只处理 Python 的 `shutdown_if_configured`）。
- **两套「登录」在同一个页面上**：`ReverseProxyPage` 的 `LoginCard` 支持账号口令 / 管理密钥 / 管理令牌三种，并把口令明文写到 `~/.wb-switch-plus/reverse-proxy-login.json`（`reverse-proxy-service.ts` 的 `saveLogin`）。桌面端用户其实已经登录了主程序，这一层是重复且危险的。
- **Python 与 Rust 混在一个页面里**：顶部「Python / Rust」切换器会让整页 `window.location.reload()`，且两套的会话令牌不通用（代码注释已写明）。这正是 v1 D6 的灰度手段，不是最终形态。

---

## 2. 目标形态

```
┌──────────────── 主程序（WB-Switch-Plus，单进程）────────────────┐
│  Tauri shell                                                    │
│   ├─ 已有后台循环（签到 / 旅行 / 轮换 / 保活 / 更新）            │
│   └─ 进程内反代（wb-switch-proxy::ProxyHandle）                  │
│        127.0.0.1:8787  ── /health /v1/*  ← 外部客户端            │
│                        └─ /admin/api/*  ← 主项目页面（同源 inv)  │
└─────────────────────────────────────────────────────────────────┘
        ▲ 同源 SPA（dist/，tauri:// 或 dev 的 localhost:1420）
        │
   侧边栏
     ├─ 反代服务 ▾          → /reverse-proxy/*（Rust，原生页面，二级菜单）
     │    ├ 概览  ├ 请求日志  ├ 账号池  ├ 分组  ├ 出口线路
     │    ├ API 密钥  ├ CodeBuddy CLI  ├ 接入指南  ├ 模型与用量
     │    ├ 服务设置（含内置服务停机/端口）  ├ 告警通知
     │    └ 高级 ▸（管理令牌 / 后台账号 / IP 安全 / 审计 / 连接测试）
     └ 反代服务old          → /reverse-proxy-old（Python 独立 exe，原样保留）
```

三条硬约束：

1. **一个进程**：Rust 引擎不依赖任何额外进程或 `wb-reverse-proxy.exe`；Python exe 只作为「反代服务old」的可选产物存在（打包资源见本文 §4 C1）。
2. **一个 origin**：桌面端页面不再 `fetch` 跨源地址；反代由桌面端进程内提供，页面只与宿主打交道。
3. **一个身份**：控制台不再要求第二次登录；`X-Admin-Token` 的获取对页面不可见。

---

## 3. 设计

### 3.1 后端并入：`crates/wb-switch-proxy` 提升为「主项目内置服务」

- crate 不改名；**删除 `src/bin/wb-proxy.rs`**（独立进程的唯一入口）。
- 生命周期统一收到一个新模块（建议 `src-tauri/src/proxy_service.rs`，替换 `proxy_engine.rs`）：

  ```rust
  // 形状示意，不是最终代码
  pub struct ProxyService {
      handle: Mutex<Option<ProxyHandle>>,
      state:  Mutex<ServiceState>,       // Stopped / Starting / Running{port} / Failed{reason}
      token:  Mutex<Option<TokenFile>>,  // 见本文 §3.3
      cfg:    Mutex<PortSettings>,       // 期望端口 + 实际端口，见本文 §3.1.1
  }
  impl ProxyService {
      pub fn autostart(&self, app: &AppHandle);              // setup 里调用，非阻塞
      pub async fn start(&self, opts: StartOptions) -> ...;  // 幂等；内置端口冲突重试
      pub async fn rebind(&self, port: u16) -> ...;          // 改端口 = 停机 + 启动
      pub async fn stop(&self) -> ...;                       // 幂等
      pub fn status(&self) -> ServiceStatus;                 // 含 desiredPort / actualPort / portShifted
      pub fn rotate_token(&self) -> ...;                     // 前端点「轮换」时用
  }
  ```

- 自动启动点：`src-tauri/src/lib.rs` 的 `setup` 闭包里，`spawn_background_loops` 之前/之后皆可，但必须满足：
  - **不阻塞窗口呈现**（`tokio::spawn`，失败只记 `ServiceState::Failed`，不 panic、不弹窗）；
  - **截图演示模式（`WB_SWITCH_SCREENSHOT_DEMO=1`）不启动**——与现有「演示模式跑不了签到」同理，避免 README 截图时真的开监听；
  - **截图演示模式与端口冲突无关**：演示模式根本不起服务，所以不会占端口。
- 退出：`RunEvent::Exit` 里 `block_on(service.stop())`（带 2s 超时）。内置反代**没有**「退出后继续服务」这个选项——它是进程内服务，进程没了服务就没了。`SettingsPage` 里「关闭应用时自动停止反代」的开关因此只对 **Python（old）** 有意义，需要改文案或在 Rust 引擎下隐藏（见 §3.4、§4 F9）。
- 状态写盘：`StateStore::open_writable`（现状已是），写前重读磁盘的策略保留。**与 Python（old）共用同一数据目录是明确决策**（见本文 §5 D4）：因此 `ProxyService` 启动时要探测 8788 上是否有 Python 的 `/health`，有则在「服务设置」给出黄色提示，old 页面启动 Python 前要先停内置服务。
- 新增数据面端点 `/v1/usage`（v1 契约里有、Rust 未实现，本次补，见本文 §3.8）。

#### 3.1.1 端口策略：可自定义 + 冲突自动 +1

（决策来源：用户 2026-10-29「允许自定义端口，冲突自动 +1」。这是对 v1「端口冲突给明确错误、不悄悄换端口」的**有意反转**：内置服务必须自己站起来，不能因为一个被占的端口就让整个反代不可用。）

- **配置来源与优先级**（沿用 `ProxyConfig::for_user()` 的既有顺序，新增一个口径）：
  1. `WB_SERVE_PORT` 环境变量（开发/排障用，最高）；
  2. `state.json` 里的反代设置（用户在内置服务卡片里填的「期望端口」）；
  3. 默认 `8787`。
  用户改端口 = 写设置 + `rebind()`，不需要重启应用。
- **冲突处理**：按「期望端口 → +1 → +2 …」逐个尝试，**最多 20 次**（`desired..=desired+20`，且跳过 `0`、跳过 > 65535）。首个 bind 成功即用，并把 `actualPort` 与 `portShifted = true` 回给前端。
  - `ProxyHandle::start` 目前直接 `TcpListener::bind` 并把错误抛出去（`lib.rs`），改为在 `ProxyService` 层做循环，`ProxyConfig` 不动。
  - 只有「地址被占用」类错误（`EADDRINUSE`）才 +1；其它错误（权限、地址非法）直接 `Failed`，不要无意义地试 20 次。
  - 20 次都失败 → `Failed{PortExhausted}`，服务设置里给出「请手动指定端口」与当前试过的区间。
- **用户要知道端口变了**（这是自动 +1 的代价，必须显式补偿）：
  - 概览常驻状态条、服务设置卡片、接入指南、CLI 接入卡片里的 base URL **一律显示 `actualPort`**，并在 `portShifted` 时挂一条 `ToneBadge tone="warn"`：「8787 被占用，已在 8788 运行」，附「复制真实地址 / 改回固定端口」两个动作。
  - 首次发生偏移时 `toast.warning` 一次（每进程一次，不反复骚扰）。
  - `/health` 返回里加 `desiredPort` / `portShifted`，便于外部脚本与排障。
- **与 old 的端口关系**：old 固定在 8788（本文 §3.4），所以内置服务从 8787 起冲突自动 +1 时**会先撞上 old**；这属正常路径，UI 用第 2 条那套提示即可，不做特判。

### 3.2 路由与菜单（前端信息架构）

| 路由 | 页面 | 说明 |
| --- | --- | --- |
| `/reverse-proxy` | `ReverseProxyLayout`（重定向到 `/reverse-proxy/overview`） | 侧边栏「反代服务」入口 |
| `/reverse-proxy/overview` | `OverviewPanel` | 现有组件 |
| `/reverse-proxy/logs` | `RequestLogPanel` | 现有组件，5 秒轮询保留 |
| `/reverse-proxy/accounts` | `AccountsPanel` | 现成组件；「添加账号」走主项目 `OAuthLoginDialog`（v1 P6 已定，保留） |
| `/reverse-proxy/groups` | `GroupsPanel` | |
| `/reverse-proxy/egress` | `EgressPanel` | |
| `/reverse-proxy/keys` | `KeysPanel` | |
| `/reverse-proxy/cli` | `CliLinkCard` | |
| `/reverse-proxy/guide` | `GuidePanel` | |
| `/reverse-proxy/models` | `ModelsUsagePanel` | |
| `/reverse-proxy/service` | `ServiceSettingsPanel`（扩充） | 站点名称、池设置入口、**内置服务卡片：状态 / 实际端口 / 期望端口 / 停机 / 启停 / 局域网 / 数据目录** |
| `/reverse-proxy/alerts` | `AlertsPanel` | |
| `/reverse-proxy/advanced` | `AdvancedLayout` → 五个子页 | `tokens` / `users` / `security` / `audit` / `test`（见本文 §3.5 的取舍） |
| `/reverse-proxy-old` | `LegacyReverseProxyPage` | Python，整页独立（见本文 §4） |

菜单形态（两级，都在「反代服务」这一项之下）：

- 侧边栏主项「反代服务」可展开，子项 = 上面 12 个常用页 + 一个分组标题「高级」，其下 5 个子页。
- 页面内不再有「竖排 16 个 Tab」的左侧栏（`ReverseProxyPage` 现在那套 `NAV` 分组 + `TabsList`）——它和侧边栏重复。
- 保留 `Tabs` **只在**「高级」这一层内部使用（5 个子页切换），因为它们的使用频率相差一个数量级，不值得占 5 个侧边栏位。
- 页签顺序沿用现有 `NAV` 的语义分组（运行 / 账号 / 接入 / 模型 / 服务 / 运维），只是把「运维」拆成「高级」。
- 计数徽标（`overview.accounts.length` 等）从 `ReverseProxyPage` 提到 `ReverseProxyLayout`，通过 context 给菜单用；现在它们是在 `ReverseProxyPage` 里 `useMemo` 出来的。

> `src/index.css` / 侧边栏现状是「平铺 NavLink + 一段 className 复制」，加二级菜单时顺手抽一个 `SidebarNavItem` + `SidebarSubItem`，别复制第五份 className 字符串。

### 3.3 鉴权：从「页面登录」到「宿主注入」

这是本次改动里**最容易被低估**的一块。现在链路是：

```
页面 --localStorage 里的 wbt_xxx--> /admin/api/*  （服务端 AdminState.sessions 里查内存会话）
```

Rust 管理面的会话只活在进程内存（`AdminState { key, sessions }`），并且只有 `ADMIN_KEY` 一种入口。主项目页面如果还要「登录」，就得让用户在桌面端里再输入一次密码——不可接受。

**目标链路（只有两条）**：

| 场景 | 页面拿到凭据的方式 | 服务端校验 |
| --- | --- | --- |
| 桌面端（Tauri） | `invoke("proxy_admin_token")` → 内存中的短期令牌，**不落 localStorage** | 进程内令牌表（`ProxyService` 持有），有效期建议 30 分钟、启动时生成、退出时作废 |
| Python（old） | 原样不动：`LoginCard` + `X-Admin-Token` + 保存口令文件 | Python 自己的 `security.py` |

**没有第三条「浏览器经 BFF」**：`wb-switch`（`crates/wb-switch-server`）**整包移除**（决策 6），项目只保留 Tauri 桌面端 + 内置反代两个形态。BFF 之所以不需要，不是因为「加一层转发麻烦」，而是因为**根本不存在第二个宿主**。

配套改动：

1. **`wb-switch-proxy` 新增「本地回环信任」判定**：请求来自 `127.0.0.1`/`::1` 且带 `X-Proxy-Host-Token`（由宿主生成、只存进程内存）时直接视为 `admin` 角色，不要求 `/admin/api/login`。**不要**改成「凡是本地来源都放行」——同机任何程序都能访问 127.0.0.1，必须仍校验令牌。
2. 令牌下发只走 invoke：`proxy_admin_token` 命令返回 `{token, expiresAt}`；页面在每次请求前取一次（有缓存，过期刷新）。**不写磁盘、不发环境变量**。
3. `src/lib/reverse-proxy.ts` 的改动面（不大，但要仔细）：
   - `getBaseUrl()`：返回 `http://127.0.0.1:<actualPort>`，端口**由 `proxy_service_status` 返回**（本文 §3.1.1），不再用常量 8788，也不再让用户手填。
   - `getToken()`：`await ensureToken()`（invoke）。
   - 现有 32 个 `reverse-proxy.test.ts` 用例大多在断言「打到哪个路径、用什么方法」，这部分不变；需要新增的是「base 与 token 的来源」和「401 时的行为」（现在 401 → 回登录页；新形态下 401 → 自动重取一次令牌，仍 401 才报错）。
4. 移除：`LoginCard` / `ConnectView` 的登录表单（`ReverseProxyPage.tsx` 里约 300 行）、`saveLogin` / `savedLogin` / `sign-out.ts` 在反代页的用法（**`sign-out.ts` 本身保留**，账户管理页还在用）、`localStorage` 里的 `wb.rp.token` / `wb.rp.user` / `wb.rp.base` / `wb.rp.engine`（迁移：首次启动时清掉旧键，避免残留的 Python 地址把页面指向错的服务）。

### 3.4 Python 版本：入口改名为「反代服务old」

**原则：Python 那一套代码不改地活着，只是换个门牌；它与内置服务共用同一份数据目录（决策 2），由「同一时刻只启用一个」来保证不互相覆盖。**

| 项 | 决定 | 理由 |
| --- | --- | --- |
| 菜单 | 侧边栏独立一项「反代服务old」，图标区分（`Server` → `ServerOff`/`History`） | 用户明确要求「入口菜单 反代服务old」 |
| 路由 | `/reverse-proxy-old`（页面内不再有引擎切换器） | 与内置页互不影响，刷新不会带着对方的令牌 |
| 端口 | Python 让到 **8788**（`WB_SERVE_PORT` 可覆盖），内置服务占 8787 起 | 8787 是对外文档里的标准端口，应属于主项目内置服务；old 只给回滚/对比用 |
| 数据目录 | **同一份** `~/.wb-switch-plus/reverse-proxy`（决策 2：不改、不迁移，后续 Python 会移除） | 两套读写同一 `state.json`，靠互斥而不是靠分目录。互斥见本文 §5 D4。 |
| 前端代码 | `src/lib/reverse-proxy-service.ts`、`service-control.tsx`、`add-account-dialog.tsx`、`batch-scan-dialog.tsx` 只被 old 页面引用 | 打包体积不增（本来就都在） |
| 浏览器（webui） | ~~old 页面在 webui 下隐藏~~ → 不再需要判断：`wb-switch` 移除后只有桌面端窗口 | 决策 6 |
| 退场 | 不设日期。等 v1 D7 的「契约全绿 + 双跑 7 天 + 回滚验证」满足后再议 | 用户要求「暂时保留」，且后续会整体移除 |

另：`src-tauri/src/reverse_proxy.rs`（约 850 行，找 exe / 起进程 / 探测 / 提权）**整块保留**，只是它服务的对象从「反代页」变成「反代服务old 页」。命令名不改（`reverse_proxy_*`），避免一次性动太多。

### 3.5 「高级」页签在桌面端的真实可用性（按决策 3 定稿）

| 页签 | 现状 | 本次处置 |
| --- | --- | --- |
| 连接测试（test） | 501 | ✅ **保留并实现**：复用 chat 链路发一条极短请求（决策 3）。必须带「会真实消耗额度」的显式确认，并且默认不自动发。 |
| IP 安全（security） | 读配置/规则/日志都在；白名单不生效（缺 `ConnectInfo`，`keys.rs:5`） | ⏸ **隐藏，后续规划**（决策 3）。降级成「规划中」的空状态，不摆「配了没用」的开关。`into_make_service_with_connect_info` 这次**仍要接上**（便于后续只补判定，不再动 serve 形态），但不暴露 UI。 |
| 审计（audit） | 读 / 清都在 | ✅ 保留 |
| 后台账号（users） | 登录只认 `ADMIN_KEY`，多用户 PBKDF2 校验未接 | ⏸ 隐藏（内置服务不需要第二个后台账号体系） |
| 管理令牌（tokens） | 增删改都在，但令牌对宿主注入形态意义不大 | ⏸ 隐藏，等有外部工具读管理面的真实需求再放出来 |

处置原则：**宁可少一个页签，也不要摆一个点了报 501 的按钮**（v1 P6 已经因为批量扫码做过一次同样的决定）。这条写进「验收」，见本文 §9 → 所以本轮「高级」实际只剩 **连接测试 + 审计** 两项可用。

另外两个桌面端上「半残」的点：

- `KeysPanel` 的「导出密钥」→ Rust 仍是 501（v1 §11 列为中高风险）：本次**隐藏该按钮**。
- `AlertsPanel` 的「发送测试邮件」→ Rust 是 501：本次**隐藏该按钮**。
  两者都指向「去『反代服务old』操作」的说明文案，不摆废按钮。

### 3.6 `wb-switch`（`crates/wb-switch-server`）整包移除（决策 6）

项目形态从「桌面端 + 反代 + 独立 webui 服务」收敛为 **Tauri 桌面端 + 内置反代** 两个。`crates/wb-switch-server`（`main.rs` 639 行 + `api.rs` 1695 行、48 条 `/api/*`、`rust-embed` 内嵌 `dist/`）与它的 `bin wb-switch`、npm 发布链路一并删除。

**移除清单**（逐项核实过）：

| 位置 | 内容 | 处理 |
| --- | --- | --- |
| `crates/wb-switch-server/` | 整包（`Cargo.toml` / `src/main.rs` / `src/api.rs`） | 目录删除 |
| 根 `Cargo.toml` | `members` 里的 `"crates/wb-switch-server"` | 删 |
| `.github/workflows/build.yml` | `:211` 构建该 crate、`:225` 打 `npm/platform/workbuddy-switch-*` 包 | 删对应 job 步骤 |
| `.github/workflows/test.yml` | `:75 / :122 / :125 / :129` 的 `-p wb-switch-server`（fmt / clippy / test） | 去掉该包 |
| `scripts/bump-version.sh` | `:8` 把版本号同步到它的 `Cargo.toml` | 去掉该文件参数 |
| `docs/DEVELOPMENT.md` | `:130` 目录树项、npm 发布步骤 | 改 |
| `CODEBUDDY.md` | `:50` crate 清单项 | 改 |
| `docs/reverse-proxy-rust-migration-plan.md` | 9 处引用（含 D2 的 BFF 方案、P6「webui BFF」待办） | 标注作废 |

**⚠️ 一处不能顺手删**：`crates/wb-switch-core/src/modules/process.rs:107` 的 `SELF_IMAGE_STEMS` 里有 `"wb-switch"`。它服务于「关闭客户端」的自我保护（`process.rs` 注释：禁止子串命中同前缀的不同用途进程，所以用精确匹配），**指的是上游遗留的同源二进制旧名，与 `wb-switch-server` 这个 crate 无关**（crate 的 `bin` 名虽也叫 `wb-switch`，但那是另一个概念）。删掉它会让从上游版本升级上来、exe 仍叫 `workbuddy-switch` / `wb-switch` 的用户在「关闭客户端」时把自己的进程当成外部进程。**保留该行**，只在注释里补一句区分。

**同时作废的东西**：

- v1 D2 的「webui 经 BFF 连本机桌面端」（`/api/proxy/*` 转发 + `webui-token` 文件）——不再有第二个宿主，无处可转发。
- v1 §6 P6 的「webui BFF」待办、v1 §7 测试策略里的「webui BFF」行、v1 §9 验收里的「webui 经 BFF 转发」条目。

**行为上真正消失的能力**（必须写进发布说明，不是重构而是功能删除）：

1. **没有安装桌面端的机器上，不能用浏览器管账号** —— 以前 `npm i workbuddy-switch` + `workbuddy-switch serve` 起 57890 就能开网页；现在这条路没了。
2. 没有 npm 平台二进制与 npm 包发布链路。
3. `wb-switch serve / status / version` 三个 CLI 子命令消失（`main.rs` 的整个 CLI 面）。
4. **移动端宿主一并放弃**（决策 7）：不再区分 `isDesktop()` / `isWebui()` / `isMobilePlatform()`。

#### 3.6.1 前端残留清理：从三形态收敛为单一桌面端（决策 7）

`src/` **整体保留**，它仍是唯一前端（Tauri 窗口加载的就是 `dist/`）。要清的是里面为本机 HTTP 形态和移动端准备的分支。现状是三种运行形态并存，代码用 `isWebui()` / `isMobilePlatform()` 分发：

| 位置 | 现状 | 处置 |
| --- | --- | --- |
| `src/lib/api.ts:226-249` | `httpCall()`：浏览器用相对路径 `fetch` 打 `/api/*` | **删函数** |
| `src/lib/api.ts:104-198` | `ROUTES` 表（48 条 `{method, path}`） | **删**（只被 `httpCall` 查表） |
| `src/lib/api.ts:265-266` | `if (!isWebui()) return invoke(...); return httpCall(...)` | 收敛为直接 `invoke` |
| `src/lib/api.ts:79-96` | `isWebui()` / `isMobilePlatform()` / `isDesktop()` | 三个都删；`isDesktop()` 的处理见本节末尾 |
| `src/lib/api.ts` 内 8 处 `isWebui()` 分支（`:777 / :790 / :803 / :943 / :1052 / :1067 / :1074 / :1139 / :1150 / :1156`） | 全部是「浏览器不支持 X 就提前返回」 | 逐条删除，保留其 `invoke` 正常路径 |
| `src/lib/update.ts:18-19` | 自己又判了一遍 `__TAURI_INTERNALS__` | 删掉该判断 |
| `src/lib/error-report.ts:28` | `!isDemoMode() && !isWebui()` | 简化为 `!isDemoMode()` |
| `src/lib/use-visible-interval.ts:50`、`use-credit-auto-refresh.ts:72` | `if (!isWebui()) listen("main-window-visible")` | **必删**：`listen()` 只在 Tauri 环境有，恒为 true 本来就该去掉；留着等于用常量 true 包一层 |
| `src/lib/use-workbuddy-status-refresh.ts:19` | `const webui = isWebui()` 并据此初始化 `mainWindowVisible` | 改为按桌面端语义初始化（`false`，等事件给权威值） |
| `src/lib/use-update-state.ts:47`、`use-rotate-deferred-notice.ts:19`、`src/pages/AccountsPage.tsx:555/572` | `if (isWebui()) return` 早退 | 删早退 |
| `src/components/switch-account-dialog.tsx:104` | 浏览器走 `switchProgress()` 轮询代替事件 | 删该分支（桌面端有事件） |
| `src/components/export-accounts-dialog.tsx:119` | 浏览器用 Blob 下载 | 删该分支（桌面端走保存对话框） |
| `src/components/oauth-login-dialog.tsx:221` | 浏览器用 `window.open` | 删该分支（桌面端由 Rust 侧开系统浏览器） |
| `src/App.tsx:341`、`src/pages/SettingsPage.tsx:1021/2313` | `isWebui()` 决定是否渲染更新中心/卡片 | 删条件，恒渲染 |
| `src/lib/api.test.ts` | 5 个 `httpCall` URL 构造用例（含 SSH 转发端口场景） | 随被测代码一起删 |

**`isDesktop()` 的处理**：它是 20+ 处的判断入口（`requireCompanionDesktop()`、`serviceSupported()`、反代页的桌面判定等）。删完 `isWebui` 后它恒为 true，有两条路：

- **(a) 保留函数、恒返回 true**——改动面最小，但它从此是死条件，将来容易被人当成有效判断继续写。
- **(b) 彻底删掉**，把 20+ 处调用点改成直接执行——干净，但一次改动面大，容易漏。

**本计划选 (b)**（决策 7 已明确「只有桌面端版本」）：既然不做第二形态，就不该留一个永远为真的分流函数。分两批做：先删 `isWebui`/`isMobilePlatform` 与其分支，再删 `isDesktop()` 与 `requireCompanionDesktop()`。

> 顺序很关键：**第一批删完后 `npx tsc --noEmit` 必须已经能过**，第二批才是真删函数。否则一次大改后编译错误会混在一起，分不清是哪个分支删错。

**不要顺手删的**：`src/lib/demo-mode.ts` 的 `demoModeEnabled`（截图/演示模式仍要用，`WB_SWITCH_SCREENSHOT_DEMO=1` 与反代 autostart 的排除逻辑都依赖它）。`pagesDemoHostingEnabled` 与 `App.tsx` 的 `HashRouter` 分支是给 GitHub Pages 无后端演示用的，与本次无关，单独评估。

**验证方式**：清理完 `grep -r "isWebui\|isMobilePlatform\|isDesktop\|__TAURI_INTERNALS__\|workbuddy-switch serve" src/` 应只剩 0 处（或仅注释）；`npx tsc --noEmit` 通过，因为 `isDesktop()` 删掉后所有守卫都会变成编译期可见的直接调用。

### 3.7 数据面地址与「服务地址」展示

- 概览/服务设置/接入指南/CLI 卡片里展示的「服务地址」一律取 `status.baseUrl`（= `http://127.0.0.1:<actualPort>`），不再用前端 `getBaseUrl()`。
- 内置服务默认端口 **8787**（决策 1），可在「服务设置 → 内置服务」里自定义；冲突自动 +1（本文 §3.1.1）。
- 局域网开关（`0.0.0.0`）：Rust 侧 `ProxyConfig.host` 支持，但**现在没有 UI**（`ServiceControl` 只对 Python 有效）。落到「服务设置 → 内置服务」里，语义仍是「每次启动重新确认，不写进配置」（沿用 `service-control.tsx` 注释里的决定）。

### 3.8 `/v1/usage`（决策 5：本次补）

- 契约里本来就有一条数据面路由 `GET /v1/usage`（`docs/reverse-proxy-contract.md` 的 data-plane 组），Rust 侧没实现。
- 实现口径对齐 Python：只读、无鉴权（与契约一致），返回 `usage_stats` 的当前快照 + 进程启动以来的汇总；字段名沿用 `state.json` 的键，不新造字段。
- 落地位置：`http.rs` 加一条 route，数据处理放 `billing.rs`（它已经在管用量/额度口径）。
- 验收时把这条也加进「数据面 6 条 + `/v1/usage`」的对齐核对里。

---

## 4. 迁移映射表（现状 → 目标）

| # | 现状文件/符号 | 动作 | 备注 |
| --- | --- | --- | --- |
| B1 | `crates/wb-switch-proxy/src/bin/wb-proxy.rs` | **删** | 独立进程入口 |
| B2 | `src-tauri/src/proxy_engine.rs`（`ProxyEngine`） | **改名/重写为 `proxy_service.rs`** | 加 autostart、状态机、退出停机、令牌 |
| B3 | `src-tauri/src/lib.rs`（`manage(ProxyEngine)` + 3 个命令 + `RunEvent::Exit`） | 改 | 换成 `ProxyService`；注册 `proxy_service_*` / `proxy_admin_token`；Exit 里停服务 |
| B4 | `crates/wb-switch-proxy/src/admin.rs` 鉴权段 | 改 | 增加宿主令牌 → `admin` 角色（本文 §3.3） |
| B5 | `crates/wb-switch-proxy/src/admin.rs` 501 分支 | 补或隐藏（两者择一，见本文 §3.5） | 至少让页面知道「哪条不支持」 |
| B6 | `crates/wb-switch-proxy/src/http.rs` | 改 | `into_make_service_with_connect_info`（IP 白名单前置）；`/v1/usage` |
| B7 | `crates/wb-switch-server/`（整包） | **删除** | 决策 6：项目只保留桌面端 + 内置反代；见本文 §3.6 移除清单 |
| F1 | `src/App.tsx`（导航 + 路由） | 改 | 二级菜单 + 新增路由；抽 `SidebarNavItem` |
| F2 | `src/pages/ReverseProxyPage.tsx` | **拆分**为 `ReverseProxyLayout` + `pages/reverse-proxy/OverviewPage.tsx` 等薄壳 | 登录门删除 |
| F3 | `src/pages/LegacyReverseProxyPage.tsx` | **新建** | = 现 `ReverseProxyPage` 的 Python 分支 + `service-control.tsx` |
| F4 | `src/lib/reverse-proxy.ts` | 改 | base/token 来源（本文 §3.3）；401 处理 |
| F5 | `src/lib/proxy-engine.ts` | **删** | 不再有「引擎选择」 |
| F6 | `src/lib/reverse-proxy-service.ts` | 保留，只给 F3 用 | |
| F7 | `src/components/reverse-proxy/service-control.tsx` | 保留（old 用）+ 抽出一个 `BuiltinServiceCard`（Rust 用） | 两者共用 `rp-shared.tsx` 的卡片样式 |
| F8 | `src/components/reverse-proxy/service-settings-panel.tsx` | 扩 | 加「内置服务」分区 |
| F9 | `src/pages/SettingsPage.tsx`（`serviceCloseOnExit*`） | 改文案/条件渲染 | 只对 old 生效 |
| F10 | `src/lib/nav-prefs.ts` | 扩 | 新增「显示反代服务old」开关（默认开），与「关联会话」同形 |
| T1 | `src/lib/*.test.ts`（反代相关 76 例） | 过一遍 | 涉及引擎选择的断言要删/改 |
| C1 | `src-tauri/tauri.windows.conf.json`（`wb-reverse-proxy.exe` resource） | **不改**（old 还要用） | 退场时再删（v1 P7） |
| C2 | `scripts/verify-release-binary.mjs:32` | 不改 | 同上 |
| D1 | `docs/reverse-proxy-rust-migration-plan.md` | 不改历史结论，**开头加一行指向本文** | 避免两份计划打架 |
| D2 | `docs/reverse-proxy-contract.md` | 不改（自动生成） | 若补了 501 项的实现，重跑录制脚本 |

---

## 5. 关键决策

**D1（本次核心）：Rust 反代随主程序启动，且不可关闭为「独立进程」。**
理由：进程内服务若还要用户手动启动，等于保留了两套生命周期。代价是「关掉桌面端，反代就没了」——这本来就是进程内方案的必然结果（v1 D1 已选），必须在 UI 文案上说明，而不是靠 `close_on_exit` 开关假装可选。

**D2：页面身份 = 主程序身份，控制台不再有登录页。**
理由：桌面端已经做过 OAuth/账号库鉴权；再让用户输入一遍 `admin` 口令，只是为了迁就 Python 时代的实现。代价：Rust 管理面的多用户/只读角色暂时闲置（本文 §3.5），`AdminState` 的 `sessions` 机制保留给 old 与外部宿主。

**D3：Python 版本改名为「反代服务old」，端口让到 8788，代码不动。**
理由：8787 应该属于主项目；旧版只是回滚通道，不该继续占主号。代价：已按 8787 配置过客户端的老用户，如果坚持用 old，需要改回 8788（页面里给出提示与一键复制的真实地址）。

**D3b：内置服务端口可自定义，冲突自动 +1（决策 1）。**
理由：内置服务必须自己站起来。若沿用 v1「端口被占就报错」的做法，一个被占的端口就能让整个反代不可用，而这个服务已经是主功能。代价有两个，都必须显式补偿：① 客户端 base URL 可能不是 8787 了 → 所有界面与 `/health` 都暴露 `actualPort`，偏移时给明显提示；② 脚本/文档里写死 8787 的地方需要改成「读 status」。上限 20 次试错，超过即 `Failed`，不无限试探。

**D4：数据目录共用，靠「同一时刻只启用一个」保证不互相覆盖（决策 2）。**
Python（old）与内置服务同指 `~/.wb-switch-plus/reverse-proxy`，`state.json` 会被两边交替覆写（`StateStore::update` 写前重读能缓解但无法根治）。**明确接受这个风险**，因为 Python 后续会整体移除，不值得为它引入目录迁移或改 Python。互斥落到三处：
1. `ProxyService::autostart` 启动前探测 8788 上是否有 Python 的 `/health`（带 `engine` 字段），有则在「服务设置」黄条提示「old 正在运行，两边会抢同一份 state.json」；
2. old 页面点「启动」前先调 `proxy_service_stop()`，并弹一次确认；
3. 两边 `/health` 都回 `dataDir`，页面对比即可显示冲突。
**这是本文唯一一处「用户可能真的丢数据」的路径**，文案必须写清楚，不做静默处理。

**D5：二级菜单用「侧边栏两级 + 路由」，不用页内 Tab 做一级导航。**
理由：现有页内竖排 16 项已经过载；而路由化后每个页签可直达、可刷新、可被浏览器书签，也方便 old 页面与内置页面共存（同一个侧边栏两套页面）。

**D6：不做「远程模式」、不做 BFF；并移除 `wb-switch`（决策 4 + 6）。**
v1 D2 的「webui 经 BFF 连本机桌面端」**作废**。理由不是「转发麻烦」，而是**已经没有第二个宿主**：`crates/wb-switch-server` 整包删除后，能跑反代的只有 Tauri 桌面端进程。代价是把「浏览器管账号」这条使用路径整体去掉（用 npm 装 CLI、开 57890 网页的老用户会失去这个入口），所以它是**功能删除**而非重构，必须进发布说明（见本文 §3.6）。

**D8：反代宿主唯一 = 桌面端主程序，启动即监听。**
项目只保留两个形态：Tauri 桌面端（含内置反代）、Python old（回滚用）。`ProxyService::autostart` 挂在 `setup`，不需要任何点击；端口默认 8787、可自定义、冲突 +1（本文 §3.1.1）。不存在「第二个宿主去连同一个 `state.json`」的问题——唯一的共存场景是 Python old，由本文 §5 D4 的互斥处理。

**D7：本次不改协议层。**
`chat.rs` / `responses.rs` / `anthropic.rs` / `projection.rs` 一行不动。本次是**装配工程**，不是协议工程——混在一起会让回归定位变成灾难。唯一的例外是新增只读端点 `/v1/usage`（决策 5），它不碰任何已有转发路径。

---

## 6. 分阶段实施（按此顺序，每阶段独立可验收）

### I0 决策落定（✅ 已完成，2026-10-29）
七项已拍板，写入本文 §3.1.1 / §3.4 / §3.5 / §3.6 / §3.8 与 §5：
1. 端口可自定义（默认 8787）+ 冲突自动 +1（含 20 次上限与 UI 提示方案）；
2. 共用同一数据目录（接受风险，靠互斥；Python 后续移除）；
3. 连接测试保留并实现；IP 安全本次隐藏、后续规划；
4. 不做 BFF；
5. `/v1/usage` 本次补；
6. `wb-switch`（`crates/wb-switch-server`）**整包移除**，只保留 Tauri 桌面端 + 内置反代；
7. **不考虑移动端**，前端收敛为单一桌面端形态。

**验收**：✅ 本文定稿；`docs/reverse-proxy-rust-migration-plan.md` 顶部加了一行指向本文。

### I1 后端并入与自动启动（✅ 已完成，2026-10-29，commit `ee0b46c`）
- 删 `bin/wb-proxy.rs`；新增 `src-tauri/src/proxy_service.rs`（替换 `proxy_engine.rs`）。
- `setup` 里 autostart（排除截图演示模式）；`RunEvent::Exit` 优雅停机（2s 超时）。
- **端口策略**（本文 §3.1.1）：期望端口读取顺序 `WB_SERVE_PORT` → `state.json` → **8787**；`EADDRINUSE` 时 +1 重试，最多 20 次；状态回 `desiredPort` / `actualPort` / `portShifted`。
- 新增/调整命令：`proxy_service_status` / `proxy_service_start` / `proxy_service_stop` / `proxy_service_set_port` / `proxy_admin_token`；删除 `proxy_engine_*`。
- `admin.rs`：宿主令牌 → admin 角色（`with_host_token`）；顺带实现 `POST /admin/api/test`（连接测试，决策 3）。
- `http.rs`：`into_make_service_with_connect_info`（本次只接上，白名单判定后续）；新增 `GET /v1/usage`（本文 §3.8）。
- **不动** `crates/wb-switch-server`（它在 I6 整包删除，先让反代跑通再删）。

**验收**：
- `cargo test -p wb-switch-proxy` 全绿（现有 45 例 + 宿主令牌 + 端口 +1 + `/v1/usage` 用例）；
- `cargo check -p wb-switch-rust` 通过；
- 启动桌面端 → 不点任何按钮，`curl 127.0.0.1:8787/health` 返回 `engine:"rust"`、`portShifted:false`；
- 在「服务设置」改成 9000 → 自动重绑，`/health` 在 9000 响应；
- 占住 8787 后启动 → `/health` 在 8788 响应且 `portShifted:true`（UI 提示在 I3 验收）；
- 关闭桌面端 → 端口释放（`Test-NetConnection` 验证，无残留）；
- `curl 127.0.0.1:<port>/v1/usage` 返回与 Python 同字段的快照。

### I2 前端路由与二级菜单（✅ 已完成，2026-10-29）
- 新建 `src/pages/reverse-proxy/*` 薄壳页 + `ReverseProxyLayout`（含菜单、计数徽标、服务状态条）。
- `App.tsx`：路由表 + 可展开的「反代服务」+ 「高级」分组（本次只有「连接测试 / 审计」两项可见）；抽 `SidebarNavItem`。
- 删除 `src/lib/proxy-engine.ts`；`ReverseProxyPage.tsx` 的 `NAV` / `Tabs` 骨架退役。
- 现有 16 个面板组件**在这个阶段不改内容**（只改承载方式），确保「面板本身没坏」这个变量被固定。

**验收**：每个二级路由可直达、刷新不丢；`npx tsc --noEmit` 通过；面板渲染与旧页面逐页截图比对（除登录门与 Tab 栏外无差异）。

### I3 鉴权改造与内置服务卡片（✅ 已完成，2026-10-29）
- `reverse-proxy.ts`：base 取 `actualPort`、token 走 invoke（本文 §3.3）；401 自动重取一次；清掉旧 localStorage 键。
- 删除 `LoginCard` / `ConnectView` / `saveLogin` 在反代页的引用；移除全部跨源/手填地址逻辑。
- `ServiceControl` 拆成 `BuiltinServiceCard`（状态 / 实际端口 / 期望端口 / 停机启停 / 端口冲突提示 / 局域网 / 数据目录）与保留给 old 的原件。
- 端口偏移提示落地：状态条 `ToneBadge tone="warn"`「8787 被占用，已在 <port> 运行」+「复制真实地址 / 改回固定端口」。

**验收**：
- 桌面端进入 `/reverse-proxy` 直接看到概览，无登录步骤、无跨源请求（DevTools 复核）；
- 服务停机后页面给出「内置服务已停止」+「启动」按钮，启动后自动恢复，不刷新页面；
- 手动占用 8787 → 首次启动出现一次 `toast.warning`，状态条与所有 base URL 显示实际端口；
- `reverse-proxy.test.ts` 32 例全绿（改动过的断言有注释说明原因）；`npm test` 全绿。

### I4 页签取舍与 501 清理（✅ 已完成，2026-10-29）
- 按本文 §3.5：保留「连接测试」「审计」，隐藏「IP 安全」「后台账号」「管理令牌」。
- 隐藏 `KeysPanel` 的「导出密钥」与 `AlertsPanel` 的「发送测试邮件」两个 501 按钮，改成「去『反代服务old』操作」说明。
- 连接测试按钮：首次点击弹二次确认「会真实消耗额度」。

**验收**：内置页面里**任意按钮被点击都不会返回 501**（清单逐条点，清单进 docs）。

### I5 「反代服务old」（✅ 已完成，2026-10-29）
- 新建 `LegacyReverseProxyPage.tsx`（= 现 `ReverseProxyPage` 的 Python 分支 + `ServiceControl` + `AddAccountDialog` / `BatchScanDialog`）。
- 引擎切换器删除；`/reverse-proxy-old` 路由 + 侧边栏项 + `nav-prefs` 开关。
- old 的默认 base 改为 `8788`，页面显著位置显示真实地址与「回滚通道」说明。
- 互斥（本文 §5 D4）：old 启动前 `proxy_service_stop` + 一次确认；`ProxyService` 启动前探测 8788。

**验收**：两个页面可同时打开、互不干扰；同一时刻只有一个引擎在写 `state.json`（看修改时间）。

### I6 移除 `wb-switch`，前端收敛为单一桌面端（✅ 已完成，2026-10-29）
分两步，**先删后端壳、再清前端分支**——反过来做会让中间态无法编译验证。

**第一步：后端（约 0.5 天）**，按本文 §3.6 清单，顺序「先摘引用、再删目录、最后跑 CI」：
1. `.github/workflows/test.yml` 去掉 `-p wb-switch-server`（fmt / clippy / test 三处）；
2. `scripts/bump-version.sh` 去掉该 crate 的 `Cargo.toml`；
3. 根 `Cargo.toml` 的 `members` 去掉 `crates/wb-switch-server`；
4. 删 `crates/wb-switch-server/` 目录；
5. `.github/workflows/build.yml` 删构建该 crate 与打 npm 平台包的步骤。

**第二步：前端（约 1 天）**，按本文 §3.6.1 的表格逐行执行：
6. 删 `httpCall()` + `ROUTES` + `api.test.ts` 里 5 个对应用例；
7. 删 `isWebui()` / `isMobilePlatform()` 及全部 15 处分支（hooks / 组件 / 页面 / `update.ts` 自带的那份判定）；
8. 删 `isDesktop()` 与 `requireCompanionDesktop()`，20+ 处守卫改为直接执行；
9. `CODEBUDDY.md`、`docs/DEVELOPMENT.md` 更新 crate 清单与发布流程；`docs/reverse-proxy-rust-migration-plan.md` 标注 BFF / webui 相关条目作废。

**⚠️ 不要动**：
- `crates/wb-switch-core/src/modules/process.rs:107` 的 `SELF_IMAGE_STEMS` 里的 `"wb-switch"`（详见本文 §3.6，那是上游遗留二进制名的自我保护）；
- `src/lib/demo-mode.ts` 的 `demoModeEnabled`（截图/演示模式仍需要它，反代 autostart 的排除逻辑也依赖它）。

**验收**：
- `cargo metadata` 里不再有 `wb-switch-server`；`cargo check --workspace` 通过；
- `cargo fmt --check` / `cargo clippy` / `cargo test` 在剩余 crate 上全绿；
- `grep -r "isWebui\|isMobilePlatform\|isDesktop\|__TAURI_INTERNALS__" src/` 只剩 0 处（或仅注释）；
- `npm test` / `npx tsc --noEmit` / `npm run tauri build` 通过；
- CI 的 build 与 test 两条流水线各跑通一次；
- 手工确认：`workbuddy-switch serve` 之类的旧命令不再产出（npm 包与二进制都不再发布）；
- 手工确认：桌面端里导出账号、OAuth 登录、切换账号、更新检查、可见性刷新**全部走原生路径且功能不变**（这 5 条是删分支时最容易删过头的）。

**这是功能删除，必须写进发布说明**（本文 §3.6 列了 4 条消失的能力，含移动端）。

---

## 7. 测试策略（本次新增部分）

| 层 | 做法 |
| --- | --- |
| 生命周期 | Rust 单测：`ProxyService::start` 幂等、`stop` 幂等、退出后端口释放（`port=0` + 真实 bind 测） |
| 端口策略 | 占住 `desired` → 实际端口 = `desired+1` 且 `portShifted=true`；连续占住 21 个 → `Failed{PortExhausted}`；非占用类错误（权限）不重试 |
| 鉴权 | 宿主令牌：有效 → 200；过期 → 401；缺失 → 401；来自非回环地址带对令牌 → 按规则拒 |
| 数据面 | `GET /v1/usage`：字段与 Python 一致、无鉴权、不写状态 |
| 连接测试 | `POST /admin/api/test`：mock 上游成功/失败各一例；确认请求走的是极短 chat 而非 Long-context |
| 前端纯逻辑（可放 `src/lib/*.test.ts`，node 环境） | `resolveProxyBase(status)`、`shouldRefreshToken(expiresAt, now)`、`portShiftNotice(desired, actual)`、`legacyEndpoint()` 抽成纯函数 |
| 前端组件 | 现有 vitest 是 `node` + 只收 `.test.ts`，**做不到组件测试**。要么加 `jsdom` 与 `.test.tsx`（成本：配置 + 首批 2–3 个组件），要么坚持「用纯函数承载判定逻辑」。**本计划建议后者**，新增 UI 只做手工清单验证 |
| 端到端 | 手工清单：启动即服务 / 端口自定义 / 端口 +1 / 20 次耗尽 / 停机恢复 / 退出释放 / old 并行与互斥 |
| 回归 | `cargo test`（proxy 45 + core 现有）、`npm test`（160）、`npx tsc --noEmit`、`npm run tauri build` 一次通过；I6 之后工作区不再含 `wb-switch-server` |

---

## 8. 风险

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 自动启动拖慢应用启动 / 启动失败弹窗 | 用户体感变差 | `tokio::spawn` + 状态机；失败只落到服务设置里 |
| 端口自动 +1 后客户端配的还是 8787 | 外部 CLI 连不上却看不出原因 | 所有界面与 `/health` 暴露 `actualPort` + 偏移时的黄色提示 + 一键复制；文档不写死端口 |
| 端口连续被占（20 次用尽） | 反代不可用 | `Failed{PortExhausted}` + 「手动指定端口」入口；不静默重试 |
| 两引擎同写 `state.json`（决策 2 的直接代价） | 密钥/审计/池状态可能丢 | D4 的三处互斥；启动前探测 8788；页面黄条显式提示；`StateStore` 写前重读保留 |
| 令牌泄漏（本机其它程序） | 同机可读管理面 | 令牌只在内存、30 分钟过期、退出作废；不落盘、不写日志 |
| 删除 `wb-switch` 打破老用户的使用路径（无桌面端机器的浏览器管账号、npm 全局命令） | 一批用户的工作方式直接消失 | 明确写进发布说明；升级包内提示「账号管理现在只在桌面端」；I6 单独一阶段并前后各跑一次完整 CI |
| 二级菜单让导航变长 | 侧边栏拥挤 | 「高级」默认折叠；`nav-prefs` 提供「显示反代服务old」开关 |
| 拆分页面时把面板 props 改坏 | 全站反代页白屏 | I2 「只改承载、不改面板内容」，单独一阶段验收 |
| old 页面长期共存导致双份维护 | 新功能两边都要改 | 明确「新功能只进 Rust」，old 只接 P0 级修复；Python 后续整体移除 |

---

## 9. 验收标准（全部满足才算完成）

- [ ] 启动桌面端，**无需任何点击**，`127.0.0.1:8787`（或自动 +1 后的端口）已提供 `/health` 与 `/v1/*`；进程列表里没有额外进程。
- [ ] 自定义端口生效：改端口后自动重绑；占住该端口后启动，实际端口 +1 且界面明确提示。
- [ ] 关闭桌面端，端口立即释放；重新打开自动恢复。
- [ ] 侧边栏「反代服务」可展开：12 个常用子页 + 「高级」（连接测试 / 审计），全部路由直达、刷新可用。
- [ ] 进入反代页**没有登录步骤**；无跨源请求（DevTools 里所有 `admin/api` 都走 invoke 注入的 base）。
- [ ] 内置页面里不存在「点了会 501」的按钮（清单逐条验证；连接测试会真实消耗额度且有二次确认）。
- [ ] `/v1/usage` 返回与 Python 同字段的快照。
- [ ] 侧边栏「反代服务old」可用：拉起 Python exe、登录、原有功能全部可用；与内置服务互斥、不互相覆盖。
- [ ] 工作区里不再有 `crates/wb-switch-server`；`cargo check --workspace`、`cargo fmt --check`、`cargo clippy`、`cargo test` 全绿；CI 的 build / test 两条流水线通过。
- [ ] `npm run tauri build` 产出的安装包里不含 `wb-switch` 二进制；`wb-switch serve` 不再存在。
- [ ] `npm test`、`npx tsc --noEmit` 全绿。

---

## 10. 决策记录（已拍板，2026-10-29）

| # | 问题 | 结论 | 落地章节 |
| --- | --- | --- | --- |
| 1 | 端口 | 允许自定义；冲突自动 +1（最多 20 次） | 本文 §3.1.1、I1 |
| 2 | 数据目录 | 与 Python 共用同一目录，不迁移；靠互斥；Python 后续整体移除 | 本文 §5 D4、§3.4 |
| 3 | 「高级」页签 | 连接测试**保留并实现**；IP 安全**本次隐藏、后续规划**；审计保留；后台账号与管理令牌隐藏 | 本文 §3.5、I4 |
| 4 | webui / BFF | **不做 BFF**；反代唯一宿主是桌面端进程 | 本文 §3.6、§5 D6 |
| 5 | `/v1/usage` | 本次补 | 本文 §3.8、I1 |
| 6 | `wb-switch` | **整包移除**（`crates/wb-switch-server` + npm 发布链路），项目只保留 Tauri 桌面端 + 内置反代 | 本文 §3.6、§5 D8、I6 |
| 7 | 移动端 | **不考虑移动端**，只做桌面端版本：删掉 `isWebui` / `isMobilePlatform` / `isDesktop` 三套形态分流；`src/` 整体保留，仍是唯一前端 | 本文 §3.6.1、I6 第二步 |

遗留（不影响开工，记录备查）：

- IP 白名单判定（`ConnectInfo` 已接上，但 key 级白名单与 `max_ips` 仍未执行）→ 后续规划。
- 密钥导出（加密格式兼容）与告警测试邮件仍是 Python 独有 → 内置页面隐藏，需要时走 old。
- OAuth 扫码 / `travel` / `active` 在 Rust 管理面仍是 501 → 桌面端已改走主项目 OAuth 与 core 的 travel 模块，页面不再暴露这些入口。
