# 反代服务契约（自动生成）

> 由 `reverse-proxy/tools/record_contract.py` 生成，**不要手改表格**。
> Rust 重写验收时，路由、鉴权、副作用等级必须与本文一致。

- 路由总数：**76**
- 分组：accounts, alerts, audit-logs, cli, data-plane, egress, groups, health, keys, login, login-options, logout, me, model-aliases, models, oauth, overview, pool, request-logs, security, session, sessions, settings, static, test, tokens, usage, users
- 风险：`read`（只读）/ `local-write`（只改本地状态）/ `upstream-write`（会打上游、可能扣积分或发邮件）/ `destructive`（删除或覆盖）

## 1. 路由清单

### accounts

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/accounts` | admin | 是 | local-write |
| GET | `/admin/api/accounts/credits-snapshot` | admin | 否 | read |
| DELETE | `/admin/api/accounts/{aid}` | admin | 否 | destructive |
| PATCH | `/admin/api/accounts/{aid}` | admin | 是 | destructive |
| POST | `/admin/api/accounts/{aid}/actions/{action}` | admin | 是 | upstream-write |
| PUT | `/admin/api/accounts/{aid}/limits` | admin | 是 | local-write |

### alerts

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/alerts` | admin | 否 | read |
| PUT | `/admin/api/alerts` | admin | 是 | local-write |
| POST | `/admin/api/alerts/test` | admin | 是 | upstream-write |

### audit-logs

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/audit-logs` | admin | 否 | read |
| POST | `/admin/api/audit-logs/clear` | admin | 是 | destructive |

### cli

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| DELETE | `/admin/api/cli/key` | admin | 否 | destructive |
| GET | `/admin/api/cli/key` | admin | 否 | read |
| POST | `/admin/api/cli/key/rotate` | admin | 是 | local-write |

### data-plane

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/v1/usage` | none | 否 | read |

### egress

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/egress` | admin | 否 | read |
| POST | `/admin/api/egress/assign` | admin | 是 | local-write |
| POST | `/admin/api/egress/lines` | admin | 是 | local-write |
| DELETE | `/admin/api/egress/lines/{lid}` | admin | 否 | destructive |
| PATCH | `/admin/api/egress/lines/{lid}` | admin | 是 | destructive |

### groups

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/groups` | admin | 否 | read |
| POST | `/admin/api/groups` | admin | 是 | local-write |
| DELETE | `/admin/api/groups/{gid}` | admin | 否 | destructive |
| PATCH | `/admin/api/groups/{gid}` | admin | 是 | destructive |

### health

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/health` | none | 否 | read |

### keys

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/keys` | admin | 是 | local-write |
| POST | `/admin/api/keys/check-models` | admin | 是 | read |
| DELETE | `/admin/api/keys/{kid}` | admin | 否 | destructive |
| PATCH | `/admin/api/keys/{kid}` | admin | 是 | destructive |
| GET | `/admin/api/keys/{kid}/export` | admin | 否 | read |
| POST | `/admin/api/keys/{kid}/reset-usage` | admin | 是 | local-write |

### login

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/login` | none | 是 | read |

### login-options

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/login-options` | none | 否 | read |

### logout

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/logout` | admin | 是 | local-write |

### me

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/me` | admin | 否 | read |

### model-aliases

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/model-aliases` | admin | 否 | read |
| POST | `/admin/api/model-aliases` | admin | 是 | local-write |

### models

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/models` | admin | 否 | read |

### oauth

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/oauth/batch/start` | admin | 是 | upstream-write |
| POST | `/admin/api/oauth/start` | admin | 是 | upstream-write |
| DELETE | `/admin/api/oauth/{fid}` | admin | 否 | destructive |
| POST | `/admin/api/oauth/{fid}/poll` | admin | 是 | upstream-write |

### overview

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/overview` | admin | 否 | read |

### pool

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/pool/actions/{action}` | admin | 是 | upstream-write |
| GET | `/admin/api/pool/checkin-logs` | admin | 否 | read |
| POST | `/admin/api/pool/checkin-logs/clear` | admin | 是 | destructive |
| GET | `/admin/api/pool/checkin-logs/summary` | admin | 否 | read |
| PATCH | `/admin/api/pool/settings` | admin | 是 | local-write |
| GET | `/admin/api/pool/summary` | admin | 否 | read |

### request-logs

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/request-logs` | admin | 否 | read |

### security

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/security/config` | admin | 否 | read |
| POST | `/admin/api/security/config` | admin | 是 | local-write |
| GET | `/admin/api/security/logs` | admin | 否 | read |
| POST | `/admin/api/security/logs/clear` | admin | 是 | destructive |
| GET | `/admin/api/security/rules` | admin | 否 | read |
| POST | `/admin/api/security/rules` | admin | 是 | local-write |
| DELETE | `/admin/api/security/rules/{rid}` | admin | 否 | destructive |

### session

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/session` | admin | 否 | read |

### sessions

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/sessions/revoke` | admin | 是 | local-write |

### settings

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/settings` | admin | 否 | read |
| PUT | `/admin/api/settings` | admin | 是 | local-write |

### static

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/` | none | 否 | read |
| GET | `/admin` | none | 否 | read |
| GET | `/admin/` | none | 否 | read |
| GET | `/admin/assets/{filename}` | none | 否 | read |
| GET | `/favicon.ico` | none | 否 | read |

### test

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| POST | `/admin/api/test` | admin | 是 | upstream-write |

### tokens

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/tokens` | admin | 否 | read |
| POST | `/admin/api/tokens` | admin | 是 | local-write |
| DELETE | `/admin/api/tokens/{tid}` | admin | 否 | destructive |
| PATCH | `/admin/api/tokens/{tid}` | admin | 是 | destructive |

### usage

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/usage/stats` | admin | 否 | read |

### users

| 方法 | 路径 | 鉴权 | 有 body | 风险 |
| --- | --- | --- | --- | --- |
| GET | `/admin/api/users` | admin | 否 | read |
| POST | `/admin/api/users` | admin | 是 | local-write |
| DELETE | `/admin/api/users/{username}` | admin | 否 | destructive |
| PATCH | `/admin/api/users/{username}` | admin | 是 | destructive |

## 2. state.json 键

| 键 | 含义 | 写入方 |
| --- | --- | --- |
| `keys` | API 密钥（策略与用量） | keysvc |
| `users` | 后台账号（PBKDF2 口令） | security |
| `api_tokens` | 管理令牌（只存哈希） | security |
| `audit_logs` | 审计日志 | security |
| `security` | 防爆破计数与会话配置 | security |
| `ip_rules` | IP 黑白名单 | security/server |
| `ip_access_logs` | IP 拦截日志 | server |
| `ip_last_allowed` | 最近放行的 IP | server |
| `api_key_ips` | 密钥来源 IP 统计 | keysvc |
| `account_status` | 账号运行态（冷却/积分/模型冷却） | pool |
| `account_overrides` | 账号本地覆盖 | switch_store |
| `session_bindings` | 会话亲和表 | pool |
| `active` | 当前账号（本服务视图） | switch_store |
| `accounts` | 账号运行时视图（不落盘） | switch_store |
| `pool` | 池配置 | pool |
| `groups` | 分组 | groups |
| `egress` | 出口线路与绑定 | egress |
| `alerts` | 告警配置 | alerts |
| `usage_stats` | 用量统计桶 | usage |
| `checkin_logs` | 任务流水 | pool |
| `travel_states` | 旅行状态 | travel |
| `runtime_since` | 本次会话起点 | switch_store |
| `runtime_total_sec` | 累计运行秒数 | switch_store |
| `runtime_sessions` | 启动次数 | switch_store |
| `model_aliases` | 模型别名 | server |
| `secret` | 站点密钥 | settings_store |
| `settings` | 站点设置 | settings_store |

## 3. 录制规则

- **只读端点**（`read`）可直接采样响应，用于 golden 对比。
- **local-write / destructive** 只记录请求形状，不在真实数据上执行。
- **upstream-write** 不录制真实调用（会扣积分/发邮件）：用本地 mock 上游录制协议层。
- 数据面样例见 `tests/fixtures/proxy/dataplane/`（本地 mock 生成，不消耗额度）。

## 4. 已知缺口

- 管理端点的响应 JSON Schema 未冻结：FastAPI 的 `/openapi.json` 只暴露 6 条数据面路由，管理路由是运行时注册的；只能靠后续 fixture 采样补齐。
- 管理面鉴权失败路径（401/403/429）未覆盖，P5 落地时按安全不变量补。
- 扫码登录只冻结状态机（state → poll → token），不录浏览器交互。

## 5. 数据面 golden 样例

| 样例 | 覆盖点 |
| --- | --- |
| `chat_cli` | CLI 流量：透传指纹头 + CLI 独有字段 + 11128 提示词改写 |
| `chat_desktop` | 桌面端流量：保持旧行为（桌面 UA、丢弃 CLI 字段） |
| `responses_stream` | Codex 的 /v1/responses：chat SSE → Responses SSE |
| `anthropic_stream` | Claude Code 的 /v1/messages：chat SSE → Anthropic SSE |
| `chat_error_11128` | 上游 400 / code 11128：客户端应收到可读错误与 wb_kind |
| `chat_incomplete_stream` | 上游没有终态帧：必须报 incomplete_stream，不能当成功 |

## 6. 如何重新生成

在 `reverse-proxy/` 下（venv 里要有 fastapi / httpx / pytest）：

```bash
./.venv/Scripts/python.exe tools/record_contract.py    # 路由表 + 本文件
./.venv/Scripts/python.exe tools/record_dataplane.py   # 数据面 golden
```

两个脚本都离线运行：前者用临时目录建空 store，后者用 MockTransport 拦上游，
**不会**消耗账号积分、不会写真实 state.json。
