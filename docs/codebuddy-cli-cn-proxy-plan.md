# CodeBuddy CLI 国内版反代 —— 实施计划

> 状态：G1–G9 全部完成（2026-10-29）；仅剩 P2 的 SPA 入口未做
> 范围：让 **CodeBuddy CLI（国内版）** 通过本机反代访问上游，账号由反代账号池在请求粒度调度，
> 换号不再需要改 `settings.json` + 重启 CLI。
> 参考实现：`D:\webProject\dsh-native-tui\src\codebuddy`（TypeScript，2026-10 实测协议）
> 落点：`reverse-proxy/` 内扩展，复用账号池 / 密钥 / 管理面。

---

## 1. 背景与目标

### 1.1 现状

| 维度 | 现在怎么做 | 痛点 |
| --- | --- | --- |
| CLI 鉴权 | `crates/wb-switch-core/src/modules/codebuddy_cli.rs` 把所选账号的 **真实 access token** 写进 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN`（Windows）或通过 `apiKeyHelper` 输出（macOS/Linux） | 换号必须改写配置 + 关掉正在运行的 CLI |
| 反代数据面 | `POST /v1/chat/completions` → `PoolMiddleware` 选号 → `core/converter.py` 转发 `copilot.tencent.com/v2/chat/completions` | 这条链路是给**外部 OpenAI 客户端**设计的，请求体按白名单裁剪、头部按桌面端指纹伪造，与 CLI 真实形状有偏差 |
| 反代控制面 | 账号池 / 多密钥 / IP·模型白名单 / 额度 / 管理页 | 已有，可直接复用 |

### 1.2 目标

1. CLI 只需配置 `CODEBUDDY_BASE_URL` 指向本机反代 + 一个**反代专用密钥**，即可正常对话。
2. 请求级别选号：429 / 402 / 403 自动换号，会话按亲和键粘住，**CLI 不重启**。
3. 保持 CLI 原始请求语义：tools / tool_calls、`reasoning_effort` 思考、图片多模态、流式增量。
4. 国内版网关的客户端指纹校验（HTTP 400 `code 11128`）有确定处置，而不是整片 400。
5. 失败语义可诊断：业务码 → 失败类型 → HTTP 状态 / 冷却策略，用户能看懂该等、该换号还是该换模型。

### 1.3 非目标

- 不修改、不注入 CLI 二进制；不逆向其内部逻辑，只按实测协议对接。
- 不做国际版 CLI（`www.codebuddy.ai`）的接入；框架预留，后续加 profile 即可。
- 不引入数据库，状态仍落 `state.json`。
- 不替换现有外部客户端出口（`/v1/responses`、`/v1/messages` 行为不变）。

---

## 2. 参考项目给出的协议事实

来源：`dsh-native-tui` 的 `src/codebuddy/profiles.ts`、`wire.ts`、`stream.ts`、`models.ts`、`credits.ts`、`errors.ts`、`prompt.ts`。
README 明确标注：**OAuth / 模型授权 / 思考档位 / 额度字段是协议转写，未用真实账号验收；模态与图片收发已实测**（`README.md:471`）。

### 2.0 本机实测（CLI 2.177.1，2026-10-28）

用本机已安装的 `@tencent-ai/codebuddy-code@2.177.1` 指向本地抓包服务（`127.0.0.1:8899`）做了一次端到端验证：

| 结论 | 证据 |
| --- | --- |
| **接受本机 http 地址** | `CODEBUDDY_BASE_URL=http://127.0.0.1:8899/v1` 下 CLI 正常拿到模型输出，`-p` 退出码 0 |
| base URL 原样当 OpenAI baseURL，**不补 `/v2`** | 实际请求 `POST /v1/chat/completions` |
| **不校验 Authorization 形态** | `CODEBUDDY_AUTH_TOKEN=probe-key`（非 JWT 明文）直接发出 |
| 认证头来源 | `Authorization: Bearer <CODEBUDDY_AUTH_TOKEN>` |
| 出站头（实测） | `User-Agent: CLI/2.177.1 CodeBuddy/2.177.1`、`X-Product: SaaS`、`X-IDE-Type`/`X-IDE-Name: CLI`、`X-IDE-Version: 2.177.1`、`x-codebuddy-request: 1`、`X-User-Id: anonymous`、`Accept: application/json` |
| 请求体键（实测） | `model`、`messages`、`tools`、`stream: true`、`stream_options.include_usage`、`max_tokens`、`temperature`、`verbosity`、`reasoning_effort`、`reasoning_summary: auto`、`cache_retention`、`extra_vars` |

由此可以定论：**本地 http 接入可行**，`CODEBUDDY_BASE_URL` 写 `http://127.0.0.1:<port>/v1` 即可，反代不必再暴露 `/v2` 兼容路径（保留也无害）。

同时要修正一条对参考项目的读法：其 `REQUEST_SHAPE` 里「不发 `stream_options`」是**它自己客户端的形状**，不是网关限制——CLI 2.177.1 实际带着 `stream_options.include_usage` 打到同一网关上。

### 2.1 国内版端点

| 用途 | 端点 |
| --- | --- |
| Chat | `https://copilot.tencent.com/v2/chat/completions` |
| 产品配置 / 模型目录 | `GET https://copilot.tencent.com/v3/config`（不带 token 也 200，但 `data.models` 为空） |
| OAuth 建 state | `POST /v2/plugin/auth/state?platform=CLI` |
| OAuth 轮询 token | `GET /v2/plugin/auth/token?state=...`（`code 11217` = 等待中） |
| Token 轮换 | `POST /v2/plugin/auth/token/refresh`（`X-Refresh-Token`） |
| 额度 | `POST /v2/billing/meter/get-user-resource` |
| 签到 | `POST /billing/meter/daily-checkin`（`code 10001` = 今日已签） |

### 2.2 CLI 出站头（实测）

```
User-Agent: CLI/2.108.1 CodeBuddy/2.108.1
X-Product: SaaS
X-IDE-Type: CLI
X-IDE-Name: CLI
x-requested-with: XMLHttpRequest
x-codebuddy-request: 1
X-Domain: copilot.tencent.com      # 仅在 auth/token 类请求上
Authorization: Bearer <token>
```

注意：**没有** `Origin` / `Referer` / `X-Enterprise-Id`；`X-User-Id` 只发占位值 `anonymous`（见 §2.0）。版本段随安装版本走（本机为 `2.177.1`，参考项目记录的是 `2.108.1`），所以反代**不能硬编码 UA**，必须透传客户端原值。

### 2.3 请求形状（实测）

- 不发 `store`。
- `stream_options`：参考项目**自己的客户端**不发，但真实 CLI 2.177.1 会发 `{"include_usage": true}`（§2.0），网关接受 → 反代保持透传/注入，不要按参考项目删掉。
- 用 `max_tokens`，不是 `max_completion_tokens`。
- 系统提示只用 `system` 角色；`developer` 角色被国际版直接拒（`11128`），国内版同样不接受。
- 思考 = `reasoning_effort` + `reasoning_summary: "auto"` 两个字段一起发。
- 图片走 OpenAI `image_url` + base64 data URI，且只允许出现在 `user` 消息和 `tool` 结果里；其他角色带图直接拒绝（不是丢弃）。
- 单请求内联图片预算 20 MiB，超了要求 offload 最老的图片。

### 2.4 SSE 与终态

- 增量字段：`delta.content` 文本、`delta.reasoning_content` / `delta.reasoning` 思考、`delta.tool_calls[]`（`index` 分片、`function.arguments` 增量）。
- `finish_reason`：`tool_calls`/`function_call` → 工具调用；`length` → 截断；其余 → 正常结束。
- **没有终态帧 = 失败**，不能当成功收尾；空闲 5 分钟无帧视为连接已死（需 abort + 重放）。
- 空完成（无任何内容块）也要报错，不返回空回答。

### 2.5 业务错误码

| code | 含义 | 归类 |
| --- | --- | --- |
| 11102 | 模型不存在/已下线 | `no_such_model`（换号无用，刷新目录） |
| 11103 / 11133 / 11135 | 请求形状不合法 | `bad_request`（换号无用） |
| 11128 | 非官方渠道（提示词指纹校验） | `bad_request`（改写提示词后重试） |
| 11134 | 限流 | `rate_limit`（可退避重试） |
| 12153 | 确定性拒绝（OAuth） | fatal，重试无意义 |
| 11217 | OAuth 尚未完成 | pending |

HTTP 语义：401/403 → auth（刷新一次再试）；402 → quota；429 → rate_limit；404/405 → 端点不存在（可换备用 base）；5xx → unavailable。

### 2.6 提示词指纹（11128）

网关只看 **system 角色原文里的特定措辞**（不看 user / tool 结果），命中即 400 `11128`：

- 2026-10-20：`^You are an AI agent powered by .+$` 被拒；
- 2026-10-28：整段 harness briefing 被拒，只改开头一个词就通过 → **匹配是措辞级、不是语义级**。

已测可用的替换（`prompt.ts` 的 `RESTATEMENTS`）：

| 被拒片段 | 替换为 |
| --- | --- |
| `^You are an AI agent powered by .+$` | `You are a software engineering assistant.` |
| `^Tokens prefixed with @ are paths the user explicitly referenced.` | `Tokens starting with @ name paths the user referenced.` |

其余提示（工具说明、工作区规则、用户指令）**逐字保留**——整体删句会让模型丢失 briefing。

### 2.7 模型目录字段

`GET /v3/config` → `data.models[]`：

- `maxInputTokens`（>1000 才采信）、`maxOutputTokens`、`tags`（含 `text-to-image` 等生成类要过滤掉，chat 端点不收）、
- `supportsImages`（`true` 可信；`false` 只是「没承诺」）、`disabledMultimodal`（`true` 优先，禁用图片）、
- `supportsReasoning.supportedEfforts[]` + `canDisableThinking`。

国内版种子清单（无网络兜底）：`deepseek-v4-pro` / `deepseek-v4-flash` / `glm-5.3` / `glm-5.3-flash` / `minimax-m3` / `kimi-k3-1` / `hy3` / `hunyuan-chat`。

---

## 3. 实施进度（9 项差距）

**进度：9 / 9 已完成。** G1–G9 已落地并验证（2026-10-29）；G3 的默认策略已生效、A/B 决策待定。桌面端 SPA 的「接入」卡片未做（P2 余项），命令层已就绪。

| # | 差距 | 状态 | 已完成内容 / 待办 |
| --- | --- | --- | --- |
| G1 | CLI 请求体字段被裁掉 | ✅ 已完成 | 白名单按 profile 取并集，`extra_vars` / `cache_retention` / `verbosity` 已透传 |
| G2 | 出站头是桌面端指纹 | ✅ 已完成 | CLI 的 UA / `X-Product` / `X-IDE-*` 原样透传，UA 版本不硬编码 |
| G3 | 身份头与 CLI 不一致 | ◐ 已实现，待定 | 默认保留账号 uid；`KEEP_CLIENT_USER_ID` 可切 `anonymous`，但开关是代码常量、未接配置 |
| G4 | 提示词触发 11128 | ✅ 已完成 | 按实测片段改写 system 文本；实机复现 11128 → 修复后通过 |
| G5 | 流没有空闲 / 终态判定 | ✅ 已完成 | 空闲读超时 300s、无终态帧 / 空回答显式报错；已输出内容时不重放 |
| G6 | 错误码只看 HTTP 状态 | ✅ 已完成 | `core/upstream_errors.py`：业务码 → 失败类 → 客户端状态码 + 账号级 / 模型级冷却 |
| G7 | `/v1/models` 缺能力元数据 | ✅ 已完成 | 目录 + 国内版种子并集，模型对象带上下文 / 输出上限 / 输入模态 / 思考档位 / 来源 |
| G8 | 没有 CLI 接入通道 | ✅ 已完成 | `purpose=cli` 密钥幂等签发 / 轮换 / 吊销 + `/admin/api/cli/key` |
| G9 | 换号要改配置 + 重启 | ✅ 已完成 | `codebuddy_cli` 的 proxy 模式（settings 写入 / 回滚、切号免重启）+ Tauri 命令 |

### 3.1 已完成（G1–G4）

| 项 | 改动 | 验证 |
| --- | --- | --- |
| G1 | `core/client_profile.py` 的 `CLI_EXTRA_BODY_KEYS`；`chat_completions` 按 profile 取白名单并集 | 同一份 CLI 请求体：修复前上游 9 键（`extra_vars` / `cache_retention` 丢失），修复后 11 键全在 |
| G2 | `CredentialManager` / `SwitchCredentialManager` 的 `_build_headers_from` 增 `profile` / `request_headers`，经 `merge_outbound_headers` 透传 | 修复前 UA `codebuddy2openai/2.0` 且无 `X-Product` / `X-IDE-*`；修复后为 `CLI/2.177.1 CodeBuddy/2.177.1` + 三个 IDE 头 |
| G3 | 身份头默认仍用账号 uid；`client_profile.KEEP_CLIENT_USER_ID` 切客户端原值 | 两种取值有单测；默认与修复前一致，桌面端流量零变化 |
| G4 | 新增 `core/cn_prompt.py`（两条实测 restatement），在 `filter_system_identity` `之后`对 4 条构造路径生效 | 单元 6 例；实机 `requests.log` 20:59:23 的 `/v1/messages` 请求，system 首句已改写为 `Tokens starting with @ name paths the user referenced.`，请求通过（用户确认「现在正常了」） |
| G5 | `_stream_upstream` 加空闲读超时（`STREAM_IDLE_SECONDS`，默认 300s，`WB_STREAM_IDLE` 可覆盖）；无 `finish_reason` 且无 `[DONE]` 时输出 `incomplete_stream` / `empty_response` 错误帧；超时映射 `stream_timeout` | 新增 `tests/test_stream_guard.py`（4 例）：半截流、空流、正常流、上游错误体 |
| G6 | 新增 `core/upstream_errors.py`：业务码 11102/11103/11128/11133/11134/11135 + HTTP 状态 → 失败类；错误体带 `wb_kind`；`AccountPool.record()` 支持账号级 / 模型级冷却，`select()` 按模型过滤 | 新增 `tests/test_upstream_errors.py`(9) + `tests/test_pool_model_cooldown.py`(3)：11134 只冷该模型、11128 不冷却、401 冷整号 |
| G7 | `model_metadata_entry()` / `list_model_metadata()`：上下文 / 输出上限 / `input_modalities` / `reasoning_efforts` / `source`；国内版种子按参考项目补位；`disabledMultimodal` 优先于 `supportsImages` | 新增 `tests/test_models_metadata.py`(6)：图片与思考档位、禁用多模态、异常上下文回退、种子补位、国际版不补种子、路由输出 |
| G8 | `keysvc` 增 `purpose`（只认 `cli`）与 `ensure/rotate/revoke_cli_key`；路由 `GET/POST/DELETE /admin/api/cli/key`，realm 固定 cn | 新增 `tests/test_cli_key.py`(6)：幂等、忽略通用密钥、轮换后旧 key 失效、吊销、保底密钥保护、purpose 白名单 |
| G9 | `codebuddy_cli::enable_proxy/disable_proxy`：写 `CODEBUDDY_BASE_URL` + `CODEBUDDY_AUTH_TOKEN` + internal；建 state 快照（只存 tokenHint）；proxy 模式下 `switch_active_account` 不写 token、不关进程；`install_helper` 拒绝覆盖；Tauri 命令 2 个 | `cargo test -p wb-switch-core`：767 + e2e 5 全绿；新增 5 例（env 往返、清空回 helper、地址校验、摘要默认关闭、token 脱敏）；`cargo check -p wb-switch-rust` 通过 |

验证方式：① 单元 `tests/test_client_profile.py`(8) + `tests/test_cn_prompt.py`(6) + `tests/test_upstream_errors.py`(9) + `tests/test_stream_guard.py`(4) + `tests/test_pool_model_cooldown.py`(3)；② 池调度 `tests/test_cli_pool_dispatch.py`(3，真实 `create_app` + `PoolMiddleware` + `MockTransport`)；③ 真实 CLI 抓包确认指纹头与额外字段；④ 实机 11128 复现 → 修复后通过。

全量 `tests/`：**1006 passed，无失败**（1000 + G8 6 例）。

池夹具的两个必要条件：关掉 `auto_checkin` / `auto_travel` / `auto_keepalive` / `auto_active`；不要进 `with TestClient(...)`。

### 3.2 剩余打磨（非差距项）

G1–G9 与前端入口已全部完成：Tauri 命令、webui 路由（`POST/DELETE /api/codebuddy-cli/proxy`）、`src/lib/api.ts` 映射与类型、`src/components/reverse-proxy/cli-link-card.tsx` + `ReverseProxyPage` 的「CodeBuddy CLI」页签。

前端验证：`npx tsc --noEmit` 通过；`npm test` **175 passed**（12 个文件），其中 `src/lib/cli-link.test.ts` 8 例覆盖地址归一化与三条接口契约。注：本机 `npm` 需清掉 `HTTP(S)_PROXY` 环境变量才能连上 registry（否则被隧道 403）。

### 3.3 已关闭项（实测一致，不要再改）

| 项 | 结论 | 处置 |
| --- | --- | --- |
| 本机 http base 能否用 | 可用，`-p` 退出码 0 | 无需自签 https |
| base 是否再拼 `/v2` | 不拼，实际打到 `/v1/chat/completions` | 只暴露 `/v1` 即可，`/v2` 别名可选 |
| 是否校验 token 形态 | 不校验，明文密钥可用 | 普通反代密钥即可 |
| `stream_options` | CLI 自带 `include_usage`，网关接受 | 保持注入；参考项目「不发」是其自有客户端形状 |
| `reasoning_effort` / `reasoning_summary` | 两个字段都在白名单 | 不改逻辑，补回归用例锁住 |
| 用量统计 | CLI 带 `include_usage`，上游会回 usage | 无需"关闭后估算"的兜底分支 |

---

## 4. 总体设计

### 4.1 数据面

```
CodeBuddy CLI (国内版)
  │  POST http://127.0.0.1:8787/v1/chat/completions
  │  Authorization: Bearer <cli-proxy-key>
  ▼
[FastAPI] ── IPGuard ── Metrics ── PoolMiddleware ──────────────────────────┐
                                   │ 校验 key(realm=cn) / 模型白名单        │
                                   │ 解析 affinity / 选号 / 占名额          │
                                   │ 设置 ContextVar: 账号凭据 + 客户端 profile │
                                   ▼                                        │
                          converter.chat_completions                        │
                                   │ body 规范化（profile 驱动）            │
                                   │ CN 提示词 restate（仅 cn）             │
                                   │ 出站头（CLI 指纹 + 账号身份）           │
                                   ▼                                        │
                     copilot.tencent.com/v2/chat/completions                │
                                   │ SSE                                     │
                                   ▼                                        │
                     流转换 / 错误分类 / 账号与模型冷却 ─────────────────────┘
```

关键点：**不新建第二条数据面**。`PoolMiddleware` 已经拦截 `POST /v1/*` 并注入 `REQUEST_CREDENTIAL`（`admin/pool.py:1893`），CLI 走的就是这条；新增的是「客户端 profile」这一层语义。

### 4.2 客户端 profile（新增概念）

一个轻量标识，由请求头+路径判定，只影响三件事：出站头、请求体规范化、错误文案。

| profile | 判定 | 出站头 | 请求体 |
| --- | --- | --- | --- |
| `desktop`（缺省） | 其他 | 现状 `realm.headers()` | 现状白名单 + `stream_options` |
| `cli` | `X-IDE-Type: CLI` 或 UA 含 `CodeBuddy/` 且 `x-codebuddy-request: 1` | 透传 UA / `X-Product` / `X-IDE-Type` / `X-IDE-Name` / `x-requested-with`；身份头按配置 | 不发 `stream_options`；补 `reasoning_summary`；扩展字段 |

profile 通过 `ContextVar` 传递（与 `REQUEST_CREDENTIAL` 同模式），避免改 `converter` 的每个调用点签名。

### 4.3 新增/改动模块

| 文件 | 动作 | 职责 |
| --- | --- | --- |
| `core/client_profile.py` | 新增 | 识别 CLI 流量；profile 定义；出站头合成；透传字段扩展表 |
| `core/cn_prompt.py` | 新增 | 11128 restatement 规则（默认两条 + `state.json` 可扩展），只处理 system 文本 |
| `core/upstream_errors.py` | 新增 | 业务码 → `FailureKind` → HTTP 状态/错误体/冷却维度；脱敏 |
| `core/stream_guard.py` | 新增 | 空闲超时、无终态帧判定、空完成判定、usage 缺失时的降级记账 |
| `core/converter.py` | 改 | `chat_completions` 接入 profile；`stream_options` 条件化；`reasoning_summary` 默认值；流包装；`/v2/chat/completions` 别名路由 |
| `core/system_identity.py` | 改 | 先 restate、后过滤；保留开关 |
| `admin/pool.py` | 改 | 设置 profile ContextVar；按错误码写入账号级 / 模型级冷却；未输出时换号重放 |
| `admin/realm.py` | 改 | 增加 CLI 指纹头常量与 `headers(profile=...)` 分支（不改现有桌面端行为） |
| `admin/keysvc.py` | 改 | 新增「CLI 专用密钥」签发/轮换/回收（realm 固定 cn、名称前缀可识别） |
| `admin/server.py` | 改 | `/admin/api/cli/proxy` 读写接入状态；接入开关；CLI 检测信息 |
| `admin/version.py` 或新 `admin/models_seed.py` | 改 | 国内版种子模型并入 `/v1/models` |
| `crates/wb-switch-core/src/modules/codebuddy_cli.rs` | 改 | 新增 `proxy` 认证模式：写入 base + 反代 key，切换账号不再关进程 |
| `scripts/codebuddy-cli-helper.cjs` | 改 | proxy 模式下输出反代 key，而不是账号 token |
| `src/pages/ReverseProxyPage.tsx` + `src/components/reverse-proxy/CliLinkCard.tsx` | 新增/改 | 「CodeBuddy CLI 接入」卡片：状态、开关、密钥遮蔽、重启提示 |

### 4.4 鉴权与密钥

- 反代侧新增一种"用途"标记，不是新鉴权机制：`keys[].purpose = "cli"`，默认 `realm=cn`、`models=[]`（随白名单策略）、`max_ips=1`。
- CLI 配置里放的是**反代密钥**，不是账号 token：账号 token 只存在于服务内存/账号库，泄不出去。
- 反代密钥丢了也不影响账号：管理页可一键重置，CLI 侧重新写入即可。
- **不**支持用真实账号 token 直连本反代（否则等于绕过池化与审计）。

### 4.5 桌面端接入流程

```
[开启] SPA → invoke(cli_proxy_enable)
  ├─ 确保反代进程在跑（复用 reverse_proxy.rs 启动器）
  ├─ 调 admin API 签发/取回 cli key（realm=cn）
  ├─ 读 settings.json → 备份 original
  ├─ env.CODEBUDDY_BASE_URL = http://127.0.0.1:<port>/v1
  ├─ env.CODEBUDDY_AUTH_TOKEN = <cli key>          （Windows / 两种 OS 都写，mac/linux 同时改 helper 输出）
  ├─ env.CODEBUDDY_INTERNET_ENVIRONMENT = internal
  ├─ 清理 local_storage 的 Endpoint-Cache（否则 CLI 仍走缓存端点）
  └─ 检测活着的 CLI 会话 → 提示"重启 CLI 生效"（不强制杀进程）
[关闭] 回滚到 original；令牌回收；helper 恢复原行为
```

复用 `codebuddy_cli.rs` 已有的：原子写、写入后校验、失败回滚（`restore_file`）、进程环境变量冲突检测（`CODEBUDDY_AUTH_TOKEN` / `CODEBUDDY_INTERNET_ENVIRONMENT`）。

### 4.6 错误与冷却策略

| 上游信号 | 对 CLI 返回 | 账号动作 |
| --- | --- | --- |
| 401/403 | 401 `authentication_error` | 先刷新 token 重试一次；仍败 → 冷却 5 分钟 |
| 402 / code 11134 | 429 `insufficient_quota` / `rate_limit_error` | 402 → 冷却 30 分钟；11134 → **模型级**冷却 + 换号重试 |
| code 11102 | 404 `model_not_found` | 该账号该模型标记不可用；不换号 |
| code 11103/11128/11133/11135 | 400 `invalid_request_error`（附中文提示） | 不冷却、不换号（换号无用） |
| 5xx / 连接失败 | 502/503 | 换号重试一次（未输出内容时） |
| 无终态帧 / 空闲超时 | SSE error + 结束 | 账号记一次失败；未输出时换号重放 |

---

## 5. 分阶段实施

### P0 —— 协议层（无 UI，手工可验证）

1. ✅ `core/client_profile.py`：CLI 判定 + 出站头合成 + 透传字段表。
2. ✅ `core/cn_prompt.py`：两条实测 restatement（接在 `filter_system_identity` **之后**，避免改写出的中性句被删）。
3. ✅ `core/converter.py`：字段白名单补 `extra_vars` / `cache_retention` / `verbosity`；`stream_options` 保持注入；profile 透传 CLI 头。
4. ✅ `core/upstream_errors.py`：业务码分类 + 错误体 + 冷却维度，接进 `admin/pool.py`（= G6）。
5. ✅ 空闲超时 + 未终态判定（并入 `core/converter.py` 的 `_stream_upstream`，= G5）。
6. ✅ 手工接入：管理页建 key（realm=cn）→ `CODEBUDDY_BASE_URL=http://127.0.0.1:8787/v1` → 跑 CLI（本机 http 已实测可行，§2.0）。

**验收**：CLI 能连续对话、工具调用、流式思考；11128 已不再触发（G4，实机验证）。

### P1 —— 接入层

1. ✅ `keysvc` 的 `purpose=cli`：签发、轮换、回收、审计日志标记。
2. ✅ `/v1/models` 合并国内版种子清单 + 目录字段（模态、思考档位、上下文）。
3. 用量记账：CLI 自带 `stream_options.include_usage`，上游会回 usage；缺失时用本地估算兜底并标记 `estimated`。
4. `/admin/api/cli/status`：CLI 是否安装、版本、settings 路径、当前是否指向本反代。

**验收**：管理页能看到 CLI 请求的账号、模型、耗时、错误码分布。

### P2 —— 桌面端接入

1. ✅ `codebuddy_cli.rs`：`authMode: proxy`；写入/回滚；切换账号免重启；状态结构体加 `proxy` 字段。
2. ~~`helper.cjs`~~ 不需要改：proxy 模式把密钥写进 settings 的 `env.CODEBUDDY_AUTH_TOKEN`，CLI 读 env 优先于 helper。
3. ✅ Tauri 命令 + `src/lib/api.ts` 适配（`enable/disable_codebuddy_cli_proxy`，双通道；webui 用 DELETE 无 body）。
4. ✅ SPA「CodeBuddy CLI 接入」卡片（`cli-link-card.tsx`，含开启 / 关闭 / 换密钥 / 重启提示）。

**验收**：应用内一键开启 → 新开 CLI 直接可用；点「切换账号」后当前会话下一次请求即换号；关闭后完全恢复原状。

### P3 —— 打磨与观测

1. 错误文案与排障文档（含 11128、端口占用、密钥失效）。
2. 告警：CLI 专用密钥异常调用、连续 11128、池内无可用账号。
3. README / PORTING / docs 更新；`build-exe.py` 重打包。

---

## 6. 关键设计决策

**D1：用专用反代密钥，而不是透传账号 token。**
否则 CLI 直接持有账号凭据，池化、审计、失效处置全部失效；且账号轮换后 CLI 必须重启。

**D2：透传 CLI 指纹，而不是继续伪造桌面端 UA。**
11128 是提示词级校验，但网关同时看头；保留真实客户端特征是最小惊讶做法。桌面端流量不受影响（profile 隔离）。

**D3：提示词按「实测片段改写」，不做通用删句。**
参考项目两次实测都表明匹配是措辞级；删句会丢模型 briefing。保留现有 `filter_system_identity` 仅作为开关兜底（默认仍开，观察后再决定是否关）。

**D4：restatement 规则可配置。**
新指纹只会继续增加，硬编码意味着每次都要发版。默认两条来自实测，`state.json` 可追加（管理页只读展示 + 导入）。

**D5：只在「尚未向客户端输出内容」时换号重放。**
半截流重放会让 CLI 看到重复文本 / 重复 tool_call，比失败更糟。

**D6：不主动杀 CLI 进程。**
proxy 模式下换号不需要重启；强行关进程属于破坏性操作，只在用户明确点「重启 CLI」时做。

---

## 7. 风险与待实测项

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| ~~CLI 是否接受 http + 本机 base~~ | **已验证可行**（§2.0，CLI 2.177.1，`-p` 退出码 0） | 无需处置；接入时保留一次自检即可 |
| ~~base URL 是否会再拼 `/v2`~~ | **已验证不拼**（§2.0，实际打到 `/v1/chat/completions`） | 反代写 `.../v1` 即可，`/v2` 别名可选 |
| ~~CLI 是否本地校验 token 形状~~ | **已验证不校验**（§2.0，明文 `probe-key` 可用） | 普通反代密钥即可 |
| `CODEBUDDY_AUTH_TOKEN` 被系统环境变量覆盖 | 现有代码已有检测 | 复用现有告警 |
| Endpoint-Cache 覆盖 settings | `local_storage/entry_933d...` 会写回进程环境 | 接入/回滚时同步清理（已有 `sync_cli_runtime_cache`） |
| `X-User-Id` 该发谁 | CLI 只发 `anonymous`，现有反代覆盖成 uid | A/B：保留 uid（配额归属）vs 还原 anonymous；profile 开关 |
| `extra_vars` / `cache_retention` 是否被网关需要 | 白名单会丢；是否影响风控/缓存命中未知 | 先透传并观察 11103/11128 与缓存命中；留开关可关 |
| 模型级 `url` 覆盖 env | CLI 的 `resolveModelBaseURL` 里 `models[].url` 优先于 `CODEBUDDY_BASE_URL`；本机内置配置无 url，远程配置可能带 | 接入前检查产品配置；必要时改用无 url 的模型 id |
| 图片大请求 | CLI 可能一次带多图 | 沿用现有 32 MiB 体上限 + 明确 413 文案 |

---

## 8. 测试计划

### Python（`reverse-proxy/tests/`）

- `test_client_profile.py`：CLI 头识别、出站头合成、字段扩展；桌面端流量零变化。
- `test_cn_prompt.py`：两条 restatement 命中/不命中、幂等、仅 system 生效、可配置追加。
- `test_upstream_errors.py`：每个业务码 → 状态码/错误体/冷却维度；`msg` 不参与判定。
- `test_stream_guard.py`：空闲超时、无终态、空完成、`[DONE]` 顺序、半截流不重放。
- `test_cli_key.py`：purpose=cli 的 realm 约束、IP 限制、回收后 401。
- 回归：现有 102+ 用例全绿（尤其 `test_account_pool.py`、`test_responses_regressions.py`）。

### Rust

- `codebuddy_cli.rs`：proxy 模式写入/回滚/校验失败回滚、切换账号不关进程、与旧 helper 模式互斥。

### 手工验收（真实国内版账号）

1. 纯文本多轮 + 中断重发。
2. 工具调用（读文件 / 执行命令）完整闭环。
3. 思考模型（`deepseek-v4-pro` 等）能看到 reasoning 流。
4. 图片输入。
5. 触发 429 → 自动换号，CLI 无感。
6. 触发 11128 → 返回可读 400，提示"渠道校验"。
7. 拔网 / 上游挂 → 502 且账号进入冷却。
8. 关闭接入 → CLI 恢复直连。

---

## 9. 验收标准

- [ ] CLI 全程配置只需 base URL + 一个反代密钥，无需账号 token。
- [ ] 账号池轮换对 CLI 透明，切换账号不重启 CLI。
- [ ] 桌面端外部客户端（Codex / Claude Code / Cherry Studio）行为零回归。
- [ ] 11128、11102、11134 三类错误各有确定的用户可读文案与冷却动作。
- [ ] 反代模式下账号库增删 / 换号 / token 刷新全部热生效。
- [ ] `npm run verify:release` 通过；反代 exe 重新打包并随包分发。

---

## 10. 剩余待确认问题

已由 §2.0 关闭：本机 http 是否可用、是否补 `/v2`、是否校验 token 形态、`stream_options` 是否被网关拒。

1. CLI 除 `/v1/chat/completions` 外是否还会打 base 上的其他路径（`/models`、`/embeddings`）？一次纯文本对话只抓到 chat 路径，工具调用 / 图片 / 子 agent 路径尚未覆盖。
2. `X-User-Id: anonymous` 是否需要替换成账号 uid？现有反代会覆盖成真实 uid，需 A/B 看风控与配额归属。
3. `extra_vars.harnessMetadata`（含 `harnessVersion` / `stepIndex`）是否参与上游判定？
4. 远程产品配置是否会给模型下发 `url`（会静默盖掉 `CODEBUDDY_BASE_URL`）？
5. `X-Domain` 到底该发 `copilot.tencent.com` 还是账号里的 `www.codebuddy.cn`？
6. 是否需要在反代页展示「CLI 当前实际请求的账号」，方便排障？

---

## 附：参考实现 → 本项目落点

| 参考（TypeScript） | 本项目（Python / Rust） |
| --- | --- |
| `profiles.ts`（端点 / 头 / 种子模型） | `admin/realm.py` + `admin/models_seed.py` |
| `wire.ts`（请求体构造） | `core/client_profile.py` + `core/converter.py` |
| `prompt.ts`（11128 改写） | `core/cn_prompt.py` |
| `stream.ts` + `sse.ts` | `core/converter.py` 流处理 + `core/stream_guard.py` |
| `errors.ts` | `core/upstream_errors.py` |
| `models.ts` | `converter._load_models_from_workbuddy` + 种子并集 |
| `auth.ts`（OAuth / 轮换） | 已有 `wb-switch-core` OAuth + `converter._refresh`，不重写 |
| `credits.ts` | 已有 `admin/pool.py` billing / 签到 |
| `adapter.ts`（重试 / 目录 TTL） | `admin/pool.py` 调度 + `admin/wb2api.py` 状态 |
