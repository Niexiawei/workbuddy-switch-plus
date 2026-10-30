/**
 * 反代服务管理端 API 客户端（codebuddy2api + switch 账号库）。
 *
 * **传输层是进程内通道，不是 HTTP**：管理面已不在 HTTP 上（数据面是唯一对外
 * HTTP 面，见 docs/reverse-proxy-internal-plane-plan.md 附录 C）。`request()`
 * 经 `setAdminChannel` 装配的通道（Tauri invoke → Rust tower oneshot）调用
 * admin router；`4xx/5xx` 语义原样保留（`RpError.status`），组件层零改动。
 *
 * 前端不再持有任何地址与凭据：base/token 概念已随管理面 HTTP 一起退役，
 * 旧 localStorage 键（wb.rp.token / wb.rp.base / wb.rp.engine）不再读写。
 */

import { invoke } from "@tauri-apps/api/core";

export class RpError extends Error {
  status: number;
  constructor(message: string, status = 0) {
    super(message);
    this.name = "RpError";
    this.status = status;
  }
}

/**
 * 管理面通道：`(method, path, body)` → `{status, body}`。
 *
 * path 是 `rp.*` 语义的相对路径（如 `overview`、`keys/k1`），**不带**
 * `/admin/api` 前缀——admin router 未经 nest，前缀在 Rust 侧也不存在。
 * `status >= 400` 由本模块的 `request()` 抛 `RpError`；通道级故障（服务未运行）
 * 以 reject 形式出现，转成 `status === 0` 的 RpError。
 */
export type AdminChannel = (
  method: string,
  path: string,
  body?: unknown,
) => Promise<{ status: number; body: unknown }>;

let channel: AdminChannel | null = null;

/** 由布局层一次性装配；传 null 回到「通道未就绪」（所有请求抛 status 0）。 */
export function setAdminChannel(next: AdminChannel | null): void {
  channel = next;
}

/** 供测试与排查用：当前通道是否已装配。 */
export function adminChannelReady(): boolean {
  return channel !== null;
}

async function request<T>(
  path: string,
  options: { method?: string; body?: unknown; auth?: boolean } = {},
): Promise<T> {
  const call = channel;
  if (!call) throw new RpError("管理面通道未就绪", 0);
  let reply: { status: number; body: unknown };
  try {
    reply = await call(options.method ?? "GET", path, options.body);
  } catch {
    // 通道级故障（服务停机 / invoke 失败）：与旧「连接不上」同一文案。
    throw new RpError("无法连接反代服务，请确认它已启动", 0);
  }
  if (reply.status >= 400) {
    throw new RpError(extractDetail(reply.body) || `请求失败（HTTP ${reply.status}）`, reply.status);
  }
  return reply.body as T;
}

/** 布局层用的默认通道实现：直接走 Tauri 命令。 */
export function defaultAdminChannel(): AdminChannel {
  return (method, path, body) =>
    invoke<{ status: number; body: unknown }>("proxy_admin_request", {
      method,
      path,
      body: body ?? null,
    });
}

function extractDetail(data: unknown): string {
  if (!data || typeof data !== "object") return "";
  const detail = (data as { detail?: unknown }).detail;
  if (typeof detail === "string") return detail;
  if (detail && typeof detail !== "object") return "";
  const err = (detail as { error?: { message?: string } }).error;
  if (err && typeof err.message === "string") return err.message;
  return JSON.stringify(detail);
}

// ── 类型 ──────────────────────────────────────────────────────────────
/**
 * 服务端的上游版本代号。**刻意不用前端的 WbVariant（cn/ai）**：
 * 服务端落盘的字段就叫 realm，而且它额外有 `ai` 这个历史别名需要归一化。
 * 界面上做选择用 WbVariant，发请求前一律过 `realmOfVariant()` 转换。
 */
export type RpRealm = "cn" | "global";

/**
 * 批量扫码里的一格。
 *
 * 服务端返回的是 `{slot, status, ...flow}` 的展开形态 —— 成功时 flow 的
 * `id/url/expires_at/interval/realm/region` 直接铺在同一层，所以这里的 `flow_id`
 * 其实就是 flow 的 `id`。**失败格只有 `slot` 和 `error`**，没有任何 flow 字段。
 *
 * 三种状态刻意不合并：`ready` 是拿到链接、`failed` 是这次没拿到、
 * `done` 是扫完了（由前端在轮询成功后自己标，服务端不知道用户扫没扫）。
 */
export type RpBatchScanItem = {
  slot: string;
  status: "ready" | "failed";
  id?: string;
  url?: string;
  expires_at?: number;
  interval?: number;
  realm?: string;
  region?: string;
  error?: string;
};

/** 国际版可选地区（派发给服务端的 IOS2 短码，与服务端 REGION_CODES 一致）。 */
export const RP_REGIONS: ReadonlyArray<{ code: string; label: string }> = [
  { code: "HK", label: "中国香港" },
  { code: "MO", label: "中国澳门" },
  { code: "SG", label: "新加坡" },
  { code: "TH", label: "泰国" },
  { code: "PH", label: "菲律宾" },
  { code: "MY", label: "马来西亚" },
  { code: "ID", label: "印度尼西亚" },
];

export type RpSession = { username: string; role: "admin" | "viewer" | string; via?: string };

/**
 * 界面档位 → 服务端 realm。
 *
 * 两套命名不同不是笔误：前端从桌面端继承的是 `cn/ai`，服务端落盘字段叫
 * `realm` 且取值 `cn/global`。转换只在这一处做，别在各组件里散着写三目。
 */
export function realmOfVariant(variant: string | undefined): RpRealm {
  return variant === "ai" || variant === "global" ? "global" : "cn";
}

/** 服务端 realm → 界面档位（展示用）。 */
export function variantOfRealm(realm: string | undefined): "cn" | "ai" {
  return realm === "global" || realm === "ai" ? "ai" : "cn";
}

export type RpAccount = {
  id: string; name: string; enabled: boolean; active: boolean; realm?: string;
  nickname?: string; uid?: string; status?: string;
  pool_state?: string; remaining?: number | null; credits_updated?: number;
  credits_stale?: boolean; today_checked_in?: boolean; cooldown_until?: number;
  /** 这一版有没有签到体系。国际版（workbuddy.ai）没有，界面要写「该版本无签到」。 */
  checkin_supported?: boolean;
  /** 版本的中文名（国内版 / 国际版），由服务端给出，界面不自己推。 */
  realm_label?: string;
  last_error?: string | null; expires_at?: number; expired?: boolean;
  refresh_available?: boolean; request_count?: number;
  /**
   * 主项目账号库的「需重新登录」判定（`accounts.json` 的 `needs_relogin`）。
   *
   * 反代选号与账户管理必须用同一判据：这个号为真时池状态是 `invalid`、不参与调度。
   * 界面拿它在池里单独提示「去账户管理重新登录」，而不是笼统地说「凭据异常」。
   */
  needs_relogin?: boolean;
  needs_relogin_reason?: string;
  /** 上游已确认凭据失效（刷新后仍 401）。与 needs_relogin 分开展示，处置不同。 */
  auth_invalid?: boolean;
  /** 账号库里是否还留有可用 token。为假说明这条记录已经空了。 */
  has_token?: boolean;
  /**
   * 派猫猫旅行的展示档位（与服务端 travel_label 一一对应）：
   * traveling 旅行中 / finished 今日已派 / no-buddy 无 Buddy / untraveled 未旅行。
   */
  travel_label?: "traveling" | "finished" | "no-buddy" | "untraveled" | string;
  /** 这条旅行记录是不是今天的。为假表示是上一次行程的残留（还没领到手）。 */
  travel_same_day?: boolean;
  travel_location?: string | null;
  /** 到达时间，上游 ms / s 两种形态都有，展示前要归一化。 */
  travel_arrive_at?: number | null;
  travel_reward?: number | string | null;
  travel_skip?: string | null;
  /**
   * 账号级流量整形。三项并存于同一个对象里（服务端 `throttle.limits_of` 的形状），
   * 缺省时分别读作 `1 / 0 / 0`，即「等权、不限并发、不限速」。
   *
   * `weight: 0` 有独立语义——不参与自动轮转，但手动指定仍可用。别把它
   * 和缺省混为一谈：缺省是 1。
   */
  weight?: number;
  max_concurrency?: number;
  min_interval?: number;
  /** 此刻正在打到这个号上的请求数。用来判断「是我设太小了还是号真的卡了」。 */
  inflight?: number;
  /** 上一次请求的时间戳（秒）。最小间隔是从它开始算的。 */
  last_request?: number;
  /**
   * 最近一次「对话活跃上报」被证明生效的日期（YYYY-MM-DD）。没生效过为 null。
   *
   * 注意这里的口径：只有回读到连登天数 > 0 才写它。上报拿到 200 但事件被上游
   * 静默丢弃时不写——那种情况下当天还要按 30 分钟节拍重试，写下去等于把
   * 自愈的路关死。所以「今天没上报过」和「今天上报失败」不是一回事，见 active_error。
   */
  active_date?: string | null;
  /** 今天是否已经确认生效**。跨日会自然变回 false（与 travel_same_day 同一口径）。 */
  active_today?: boolean;
  /** 回读到的 growth 连登天数。上报成功但它是 0 就是被上游静默丢弃了。 */
  active_streak?: number | null;
  /**
   * 上一次失败、或「报了但没能确认生效」的原因。
   *
   * 活跃上报的失败写的是这个字段，而不是通用的 last_error——所以卡片上要
   * 单独把它显示出来，否则「一直没点亮」这个状态在界面上是看不见的。
   * 成功会被服务端清空。
   */
  active_error?: string | null;
  /**
   * 当天的终局跳过（目前只有国际版账号）。这类账号今天不会再被尝试，
   * 因为重试一万次也不会变好、只会把上限 500 条的流水塞满同样的失败记录。
   * 跨日自动失效，所以界面应显示「今日不再尝试」而不是一条看起来待重试的失败。
   */
  active_skip?: string | null;
  /**
   * ── 上游 workbuddy2api 的权威冷却（模型级）────────────────────────
   *
   * 与上面的 `cooldown_until` 是两个不同的来源，别混：
   * · `cooldown_until` 是我们自己按错误码猜的（401/403 → 5 分钟，402/429 → 30 分钟），
   *   整个账号一个时间戳，没有模型维度；
   * · 这几个 `up_*` 字段是上游状态机给的：哪个模型被限流、各自几点恢复、为什么。
   *
   * `null` 表示上游读不到或没配（不是「没有冷却」）。界面必须把这两种
   * 情况分开——把「读不到」画成「没有冷却」是在替上游下结论。
   */
  up_cooling?: boolean | null;
  /** 账号级冷却剩余秒数。上游给的是快照**，服务端已按绝对时刻重算过。 */
  up_cool_remaining?: number | null;
  up_reason?: string | null;
  up_until?: number | null;
  /** 模型级限额台账。这是这个功能的重点：账号级说不出「哪个模型、几点恢复」。 */
  up_models?: RpUpstreamModel[];
  /** 连续失败次数（连败降权用）。它与限流退避都会显示成「冷却中」，但处置不同。 */
  up_consecutive_fails?: number | null;
  up_degrade_until?: number | null;
  up_disabled?: boolean | null;
  up_manual_disabled?: boolean | null;
};

/** 上游单个被限流模型的台账行（对应 Go 的 pool.RateLimitedModel）。 */
export type RpUpstreamModel = {
  model?: string;
  /** 该模型的独立冷却截止（可能已被上游的 soft_rate_max 截断过）。 */
  until?: number | null;
  /** 上游「将在…重置」的原始墙钟，未截断时与 until 同值。 */
  reset_at?: number | null;
  /** 还要等多少秒。由服务端按绝对时刻重算，可能为 null（时刻已过）。 */
  remaining_sec?: number | null;
  /** 触发原因。6004 开头 = 限流（等一会儿就好）；11102 开头 = 没开通这个模型（等不好）。 */
  reason?: string;
};

/**
 * 调度方式：
 * - manual      只用「当前账号」，绝不轮到别的号
 * - round_robin 轮流用（默认），把请求摊到所有可用账号上
 * - least_used  每次挑累计请求数最少的号
 */
export type RpRouting = "manual" | "round_robin" | "least_used";

export type RpPool = {
  routing?: RpRouting | string;
  auto_checkin?: boolean;
  checkin_time?: string;
  /** 每日自动派猫猫旅行。开启后会对账号发起真实写请求，所以默认关闭。 */
  auto_travel?: boolean;
  travel_time?: string;
  /** token 剩余寿命少于 keepalive_hours 小时就提前刷新。 */
  auto_keepalive?: boolean;
  keepalive_hours?: number;
  /** 每日对话活跃上报。开启后会以每个启用账号的身份向上游发真实事件，默认关闭。 */
  auto_active?: boolean;
  active_time?: string;
  /** 每个账号每天上报几条 chat_request_send 事件（1–5）。 */
  active_reports?: number;
  /**
   * 上游 workbuddy2api 的地址（如 http://127.0.0.1:7863）。
   * 空串 = 不去读上游，界面就不显示模型级冷却。默认空。
   */
  wb2api_base?: string;
  /** 上游要求 Bearer 鉴权时填它的 API key；留空表示上游未设鉴权。 */
  wb2api_key?: string;
  /**
   * 账号级流量整形的默认值 —— 新建账号时抄一份过去。
   *
   * 缺省读作 `1 / 0 / 0`。`default_weight` 服务端**不接受 0**：
   * 权重 0 的语义是「这个号不参与自动轮转」，那是单个账号的意图，
   * 不该成为所有新加账号的初值（加完发现不工作，且界面不会报错）。
   */
  default_weight?: number;
  default_max_concurrency?: number;
  default_min_interval?: number;
  /**
   * 全池总并发上限：同一时刻打到上游的**总**请求数，0 = 不限制。
   *
   * 与 `default_max_concurrency` 不是一回事：后者是「每个号自己的上限」，
   * 20 个号各配 3 并发时，单号看全都合规，上游看到的却是 60 个请求同时到。
   * 这一项是给整体加一道闸，超出部分排队而不是失败。
   */
  max_total_concurrency?: number;
  /**
   * 最近一次读上游 `/status` 失败的原因；空串 = 没问题。
   *
   * 与 `up_cooling` 是两个层次：这个说「读没读到」，那个说「读到了的话有没有冷却」。
   * 读不到时 `up_cooling` 是 `null`（未知），靠这个字段才能把「未知」讲清楚 ——
   * 绝不因为读不到就退化成 `false`（显示成「没有冷却」），
   * 否则会出现那个缺陷。
   *
   * 服务端放在 `pool` 段而不是逐行账号里：这个错误是整批共享的
   *（`upstream_cooldowns()` 一次调用拿整张 uid→冷却 的表），
   * 逐行重复既冗余，又会让人误读成「每个号各有各的错」。
   *
   * ⚠️ 只在「上游读不到」时才有意义，所以必须是可选的（服务端实际总是会发，
   * 但老版本服务端 / 无上游配置时不发，可选才能让 `?? ""` 兜底成立）。
   */
  wb2api_error?: string;
  /** spread 开启后，轮到哪个号就把它钉住 spread_minutes 分钟再换下一个。 */
  spread?: boolean;
  spread_minutes?: number;
  /** spread 的驻留租约，服务端维护；只读。 */
  lease?: { account_id: string; until: number } | null;
};

export type RpCheckinLog = {
  time: number; id: string; name: string; ok: boolean;
  /** 任务类别：签到 / 派猫猫旅行 / 凭据保活 / 活跃上报。旧数据没有这个字段，按签到算。 */
  kind?: "checkin" | "travel" | "keepalive" | "active" | string;
  credits?: number;
  message: string; source: "auto" | "manual" | "batch" | string;
};

/** 一次 /v1 请求的去向：用了哪个账号、什么模型、扣了多少。 */
export type RpRequestLog = {
  time: number;
  account_id: string | null;
  account_name: string | null;
  model: string | null;
  status: number;
  ok: boolean;
  tokens: number;
  credit: number;
  duration_ms: number;
  /** 首个非空响应分片到达的耗时。流式下约等于首字延迟，非流式为 0。
   * `tok/s` 列的分母就是 `duration_ms - first_token_ms`（见 request-log-panel 的 logTps）。 */
  first_token_ms?: number;
  outcome: "success" | "upstream_error" | "no_account" | "rejected" | string;
  source: "api" | "test" | string;
  /** 走的是哪把密钥（`RpKey.id`）。null / 空 = 这次调用没走密钥（直连 / 被拒请求）。
   * 日志行本身不带分组信息，分组挂在密钥上：key_id -> RpKey.groups[] -> RpGroup.name。 */
  key_id?: string | null;
  /** 密钥的展示用短标记（如 sk-wb-abc…）。可选。 */
  key_hint?: string | null;
};

export type RpMetricItem = {
  time: number; path: string; source: string; status: number | null;
  ok: boolean; duration_ms: number; outcome: string;
};
export type RpMetrics = {
  started_at: number; completed: number; in_flight: number; succeeded: number;
  failed: number; success_rate: number | null; http_success_rate: number | null;
  avg_duration_ms: number | null; api_count: number; test_count: number;
  recent: RpMetricItem[];
};

export type RpKey = {
  id: string; name: string; hint: string; prefix?: string; enabled: boolean;
  /** 明文密钥。仅本机自用形态才有；旧条目可能为空串。 */
  secret?: string;
  expires_at: number | null; max_ips: number; ip_allowlist: string[];
  models: string[]; realm: string; quota: number; used_tokens: number;
  quota_credit: number; used_credit: number; created: number | null;
  last_used_at: number | null; ips: string[];
  /** 绑定的上游分组 id。空数组 = 不限制（可路由到全部账号）。 */
  groups?: string[];
};

export type RpOverview = {
  accounts: RpAccount[]; pool: RpPool; metrics: RpMetrics; keys: RpKey[];
  models: string[]; uptime: number;
  /** 运行时长。session = 本次进程（重启归零）；total = 累计（跨重启累加）。 */
  runtime?: { session: number; total: number; since: number; total_since: number };
  /** 客户端没带 model 时反代替它选的那一个（服务端 converter.DEFAULT_MODEL）。 */
  default_model?: string;
  checkin_logs?: RpCheckinLog[];
  request_logs?: RpRequestLog[];
  /** 上游分组清单：密钥表单据此渲染勾选，密钥列表据此把 id 显示成组名。 */
  groups?: RpGroup[];
  events: { time: number; model: string; ok: boolean; seconds: number }[];
};

export type RpToken = {
  id: string; name: string; prefix: string; scope: string; enabled: boolean;
  expires_at: number | null; created_at: number | null; created_by: string;
  last_used_at: number | null; last_used_ip: string;
};

export type RpUser = { username: string; role: "admin" | "viewer" | string };

// 安全策略 / IP 规则 / 审计日志的类型定义统一放在文件下方（RpSecurityConfig 等），
// 这里不再重复声明 —— 同名重复会让 tsc 报 Duplicate identifier。
export type RpIpLog = { ts: number; ip: string; path: string; blocked: boolean; reason: string; ua: string };

export type RpKeyInput = {
  name: string; expires_at?: number | null; max_ips?: number; ip_allowlist?: string[];
  models?: string[]; realm?: string; quota?: number; quota_credit?: number;
  /** 绑定到的上游分组；不传/空数组 = 不限制。 */
  groups?: string[];
};

/** 上游分组：账号池的一个子集，密钥可以只落在某一组里。 */
export type RpGroup = {
  id: string; name: string; description: string; enabled: boolean;
  /** 该分组自己的调度方式；空串 = 跟随全局设置。 */
  routing: "" | RpRouting;
  created: number | null;
  accounts: string[]; account_names: string[]; account_count: number;
  /** 组里已经没有可调度账号了（空组，或成员全是已被隐藏的账号）。 */
  empty: boolean;
};

export type RpGroupAccount = { id: string; name: string; pool_state: string; realm: string };
export type RpGroupsPayload = { groups: RpGroup[]; accounts: RpGroupAccount[]; routings: string[] };

/** 账号出口线路：给单个账号指定一条命名 HTTP/HTTPS 代理。 */
export type RpEgressLine = {
  id: string; name: string;
  /** 已脱敏的展示地址（`user:*@host`）；明文口令只存在服务端。 */
  url: string;
  enabled: boolean; created: number | null;
  accounts: string[]; account_names: string[]; account_count: number;
};
export type RpEgressAccount = { id: string; name: string; pool_state: string; line_id: string };
export type RpEgressPayload = {
  lines: RpEgressLine[]; accounts: RpEgressAccount[];
  /** 绑到了已删除**线路上的账号：必须重新指定一条线路才能收尾。 */
  orphans: string[];
  /** 绑到了已停用**线路上的账号：线路还在，启用回来即可恢复（当前按直连出网）。 */
  stale: string[];
};

// ── 服务设置（/admin/api/settings）──────────────────────────────────────
// site_name 已删除（附录 D）：它是 Python 网页版管理后台的 <title> 字段，
// 桌面端形态没有任何消费方。服务设置面板重组为「内置服务」卡片（P4）。

export type RpServiceSettings = {
  /** 设置加密密钥的来源（环境变量 / 本机文件），便于排查。 */
  settings_key_source?: string;
};

export type RpSettingsPatch = Record<string, never>;

// ── 告警通知（/admin/api/alerts）────────────────────────────────────────
//
// 只发信给管理员自己（封禁 / 冻结 / 积分告急），**不是用户系统**：
// 没有注册、没有找回密码、没有面向终端用户的收件人。

/** 可单独开关的告警类型。与后端 alerts.FLAG_KEYS 一一对应（有测试钉住）。 */
export type RpAlertFlag =
  | "notify_on_banned"
  | "notify_on_frozen"
  | "notify_on_credits"
  | "notify_on_upstream_disabled"
  | "notify_on_quota"
  | "notify_on_pool_empty"
  | "notify_on_pool_thin"
  | "notify_on_stale"
  | "notify_on_refresh_failed"
  | "notify_on_key_rejected";

export type RpAlerts = {
  enabled: boolean;
  /** 收件地址，多个用逗号分隔。 */
  to: string;
  host: string;
  port: number;
  username: string;
  /**
   * SMTP 口令。服务端只回掩码（`password_set` 说明是否已配置），
   * **永远拿不到明文**。所以提交时若值是掩码，必须原样退回，
   * 否则会把真口令覆盖成那串掩码字符。
   */
  password: string;
  password_set: boolean;
  sender: string;
  tls: "ssl" | "starttls" | "none" | string;
  /** 账号剩余积分低于或等于该值时提醒。 */
  credits_threshold: number;
  /** 密钥额度使用百分比阈值。 */
  quota_percent: number;
  /** 池内可用账号少于该数量时提醒。 */
  pool_min_available: number;
  /** 数据超过该分钟数未更新时提醒。 */
  stale_minutes: number;
  /** 同一密钥在该次数内被拒就提醒。 */
  key_reject_threshold: number;
  quiet_from: string | null;
  quiet_to: string | null;
} & Partial<Record<RpAlertFlag, boolean>>;

// ── 模型别名（/admin/api/model-aliases）────────────────────────────────

export type RpModelAliases = {
  /** 别名 → 真实模型名。 */
  aliases: Record<string, string>;
  /** 本机已知模型，用于校验别名指向。 */
  known_models: string[];
  default_model: string;
  /** 别名指向的模型不在已知清单里（多半拼错，或本机还没下发该模型）。 */
  unknown_targets: string[];
};

// ── 模型目录（/admin/api/models）─────────────────────────────────────
//
// 与 `RpModelAliases` 是两个端点：那个只管"别名指向谁"，这个管"有哪些模型、
// 各自什么脾气"。模型白名单选择器的「只看限时免费」靠这里的 `free`。

export type RpCatalogModel = {
  id: string;
  name: string;
  vendor: string;
  description: string;
  context_window: number | null;
  context_window_options: number[];
  max_input_tokens: number | null;
  max_output_tokens: number | null;
  max_allowed_size: number | null;
  credits: string;
  credit_rate: number | null;
  supports_tool_call: boolean;
  supports_images: boolean;
  supports_reasoning: boolean;
  only_reasoning: boolean;
  reasoning_effort: string | null;
  temperature: number | null;
  /** 上游分类标记。**判「限时免费」只认它**，不能拿 credits 文案去猜。 */
  tags: string[];
  /**
   * 是否限时免费，由后端算好。**没有 tags 就是 false**——
   * 宁可漏判也不误判：把收费模型标成免费，用户会拿它跑大活，账单出来才发现。
   */
  free?: boolean;
  /** 哪些别名指向它（一眼看出"gpt-4o 实跑 glm-5.2"）。 */
  aliases?: string[];
  /** 有多少枚密钥的白名单里含它——决定某个模型能不能安全停用。 */
  key_count?: number;
};

export type RpModelCatalog = {
  models: RpCatalogModel[];
  total: number;
  /** 限时免费的条数，后端算好；界面上的"只看免费（N）"直接用它。 */
  free_count?: number;
  default_model: string;
  aliases: Record<string, string>;
  /** 读的是哪一份产品配置（装机快照 / 运行时缓存），排查"看不到某模型"先看它。 */
  source: string;
};

export type RpSecurityConfig = {
  enabled: boolean;
  /** blacklist = 只挡 deny 规则；whitelist = 只放行 allow 规则（其余全挡）。 */
  mode: "whitelist" | "blacklist" | string;
};

export type RpIpRule = {
  id: string;
  kind: "allow" | "deny" | string;
  cidr: string;
  note: string;
  created_at: number;
};

export type RpAuditLog = {
  time: number;
  actor: string;
  action: string;
  target: string;
  detail: string;
  ip?: string | null;
};

// ── 用量统计（/admin/api/usage/stats）──────────────────────────────────
//
// 与「请求日志」的分工：那边是最近若干条的**明细**，这边是长期**分桶计数**，
// 落盘在 state.json，重启不丢。

export type RpUsageBucket = {
  /** 按天的是 `date`（YYYY-MM-DD），按小时的是 `hour`（YYYY-MM-DDTHH）。 */
  date?: string;
  hour?: string;
  calls: number;
  failed: number;
  /** 成功数 = calls - failed，后端直接给，免得前端各算各的。 */
  success: number;
  /** 失败率，0~1 的小数（后端算好四舍五入到 4 位）。 */
  failure_rate: number;
  tokens: number;
  credit: number;
  /** 平均 token / 平均耗时 / 平均首字延迟，都是后端算好的。 */
  tokens_avg: number;
  duration_ms: number;
  duration_ms_avg: number;
  first_token_ms: number;
  /**
   * 首字延迟均值。后端**只对成功的请求**求平均 —— 失败请求根本没有首字，
   * 算进去会把均值拉低，看着像"变快了"。
   */
  first_token_ms_avg: number;
};

export type RpUsageTotals = {
  calls: number;
  failed: number;
  success: number;
  failure_rate: number;
  tokens: number;
  credit: number;
  tokens_avg: number;
  duration_ms_avg: number;
  /**
   * 首字延迟均值（毫秒）。分母是**成功的**请求数 —— 失败请求没有首字，
   * 用总调用数当分母会把均值稀释成「看起来变快了」。
   * 服务端在 0.1.3 补上这个字段；旧后端返回 undefined，界面按「—」处理。
   */
  first_token_ms_avg?: number;
};

export type RpUsageStats = {
  daily: RpUsageBucket[];
  hourly: RpUsageBucket[];
  /** 按天 / 按小时各自的总量。 */
  totals: { daily: RpUsageTotals; hourly: RpUsageTotals };
  /**
   * 窗口内**按模型**的输出速率排行。分母只算真正在生成的那段时间
   * （总耗时 − 首字耗时），失败与 0 token 的请求不进样本。
   * `tokens_per_sec` 为 null = 样本不足，不能显示成 0（0 看起来像"很慢"）。
   */
  throughput?: RpUsageThroughputRow[];
  /** 实际生效的窗口，以及后端保留上限（可据此提示"只看得到最近 N 天"）。 */
  window: { days: number; hours: number; daily_keep: number; hourly_keep: number };
};

export type RpUsageThroughputRow = {
  model: string;
  calls: number;
  failed: number;
  success: number;
  tokens: number;
  tokens_per_sec: number | null;
  /** 参与速率计算的样本数。必须与速率一起显示：2 次和 200 次的数字长得一样。 */
  throughput_samples: number;
};

/**
 * 服务端回给前端的口令掩码，与 `alerts.MASKED` 必须一致。
 *
 * 这是"只回掩码"这个设计在前端唯一需要知道的东西：看到它就知道
 * **本机并不知道真口令**，因此提交时必须把这个键整个去掉，
 * 而不是把掩码当作新口令发回去。
 */
export const ALERT_PASSWORD_MASK = "********";

/**
 * 提交告警配置前，把"其实是掩码"的口令字段剔掉。
 *
 * 不做这一步的后果是静默的：提交后接口返回 200、界面显示保存成功，
 * 但服务端里存的口令已经变成 `********` 了 —— 之后所有告警邮件都发不出去，
 * 而用户以为自己只是"改了个阈值"。
 *
 * 空字符串按"用户主动清空"处理，保留；只有掩码才剔除。
 */
export function pickAlertPatch(form: Partial<RpAlerts>): Partial<RpAlerts> {
  const patch: Partial<RpAlerts> = { ...form };
  if (patch.password === ALERT_PASSWORD_MASK) delete patch.password;
  // 只读字段：服务端算出来的状态，回传没意义，还可能让 normalize 报错。
  delete patch.password_set;
  return patch;
}

// ── 接口 ──────────────────────────────────────────────────────────────
// 会话/登录端点（login* / session / logout / revokeSessions）与
// users/tokens/audit-logs 已随管理面 HTTP 一起移除（计划文档附录 B/C）；
// rp 的调用面 = P0 瘦身后 admin router 的存活端点。
export const rp = {
  overview: () => request<RpOverview>("overview"),
  poolSummary: () => request<{ accounts: RpAccount[]; pool: RpPool }>("pool/summary"),
  accountAction: (id: string, action: "refresh" | "status" | "checkin" | "travel" | "travel-claim" | "active") =>
    request<{ id: string; ok: boolean; message: string; remaining?: number
      claimed?: boolean; rewardCredit?: number | string; skip?: string | null; state?: string | null
      /** 活跃上报专属：实际发出去的事件条数 / 本轮配置条数 / 共用会话 id。 */
      sent?: number; count?: number; conversationId?: string
      /** 回读到的 growth 连登天数；streak_ok 为假表示 200 了但事件被静默丢弃。 */
      streak?: number; streak_ok?: boolean }>(
      `accounts/${id}/actions/${action}`, { method: "POST" }),
  poolAction: (action: "status" | "checkin" | "travel" | "travel-claim" | "active") =>
    request<{ results: { id: string; ok: boolean; message: string }[] }>(`pool/actions/${action}`, { method: "POST" }),
  poolSettings: (body: RpPool) => request<{ ok: boolean; pool: RpPool }>("pool/settings", { method: "PATCH", body }),
  /**
   * 账号级流量整形。**PUT 是整体语义**，三项都会被写。
   *
   * 服务端对「键缺失」的解读是「不动这一项」，所以只改权重时理论上可以
   * 只发 weight；但界面里三个框都摆着，用户看到什么就该存成什么，
   * 所以这里总是三项齐发。`0` 是合法值（不限制/不限速），调用方别把它过滤掉。
   */
  setAccountLimits: (id: string, limits: { weight: number; max_concurrency: number; min_interval: number }) =>
    request<{ ok: boolean; weight: number; max_concurrency: number; min_interval: number }>(
      `accounts/${id}/limits`, { method: "PUT", body: limits }),
  /** 任务流水。`kind` 交给服务端过滤：流水是落盘的，本地过滤会漏掉被挤出这一页的记录。 */
  checkinLogs: (limit = 200, kind?: string) =>
    request<RpCheckinLog[]>(`pool/checkin-logs?limit=${limit}${kind ? `&kind=${encodeURIComponent(kind)}` : ""}`),
  clearCheckinLogs: (kind?: string) =>
    request<{ ok: boolean; removed: number }>(
      `pool/checkin-logs/clear${kind ? `?kind=${encodeURIComponent(kind)}` : ""}`, { method: "POST" }),
  requestLogs: (limit = 200) => request<RpRequestLog[]>(`request-logs?limit=${limit}`),
  editAccount: (id: string, body: { name?: string; enabled?: boolean; active?: boolean }) =>
    request<{ ok: boolean }>(`accounts/${id}`, { method: "PATCH", body }),
  deleteAccount: (id: string) => request<{ ok: boolean; scope?: "service" | "file" }>(`accounts/${id}`, { method: "DELETE" }),
  addAccount: (body: { name?: string; credential: unknown }) =>
    request<{ id: string }>("accounts", { method: "POST", body }),

  createKey: (body: RpKeyInput) => request<{ id: string; key: string }>("keys", { method: "POST", body }),
  updateKey: (id: string, body: Partial<RpKeyInput> & { enabled?: boolean }) =>
    request<RpKey>(`keys/${id}`, { method: "PATCH", body }),
  resetKeyUsage: (id: string) => request<{ ok: boolean }>(`keys/${id}/reset-usage`, { method: "POST" }),
  deleteKey: (id: string) => request<{ ok: boolean }>(`keys/${id}`, { method: "DELETE" }),
  /** CLI 专用密钥：幂等取回（必要时服务端创建）；realm 固定 cn。 */
  cliKey: () => request<{ id: string; key: string; created: boolean; realm: string }>("cli/key"),
  rotateCliKey: () =>
    request<{ id: string; key: string; created: boolean; realm: string }>("cli/key/rotate", { method: "POST" }),
  revokeCliKey: () => request<{ ok: boolean; revoked: number }>("cli/key", { method: "DELETE" }),
  checkModels: (models: string[]) => request<{ checked: boolean; unknown: string[] }>("keys/check-models", { method: "POST", body: { models } }),

  // ── 以下 10 个方法仅供「反代服务old」（Python HTTP）的面板组件使用 ──
  // 对应的 Rust 端点已随管理面 HTTP 移除（附录 B）；内置控制台不再有这些页签。
  /** @deprecated 仅 old 页使用 */
  listTokens: () => request<RpToken[]>("tokens"),
  /** @deprecated 仅 old 页使用 */
  createToken: (name: string, scope: string, expires_at?: number | null) =>
    request<RpToken & { token: string }>("tokens", { method: "POST", body: { name, scope, expires_at } }),
  /** @deprecated 仅 old 页使用 */
  updateToken: (id: string, body: { name?: string; scope?: string; enabled?: boolean; expires_at?: number | null }) =>
    request<RpToken>(`tokens/${id}`, { method: "PATCH", body }),
  /** @deprecated 仅 old 页使用 */
  deleteToken: (id: string) => request<{ ok: boolean }>(`tokens/${id}`, { method: "DELETE" }),

  /** @deprecated 仅 old 页使用 */
  listUsers: () => request<RpUser[]>("users"),
  /** @deprecated 仅 old 页使用 */
  addUser: (username: string, password: string, role: string) =>
    request<RpUser>("users", { method: "POST", body: { username, password, role } }),
  /** @deprecated 仅 old 页使用 */
  updateUser: (username: string, body: { password?: string; role?: string }) =>
    request<RpUser & { sessions_revoked?: boolean; relogin_required?: boolean }>(`users/${encodeURIComponent(username)}`, { method: "PATCH", body }),
  /** @deprecated 仅 old 页使用 */
  deleteUser: (username: string) => request<{ ok: boolean }>(`users/${encodeURIComponent(username)}`, { method: "DELETE" }),

  securityConfig: () => request<RpSecurityConfig>("security/config"),
  setSecurityConfig: (body: RpSecurityConfig) => request<RpSecurityConfig>("security/config", { method: "POST", body }),
  ipRules: () => request<RpIpRule[]>("security/rules"),
  addIpRule: (kind: string, cidr: string, note: string) =>
    request<RpIpRule>("security/rules", { method: "POST", body: { kind, cidr, note } }),
  deleteIpRule: (id: string) => request<{ ok: boolean }>(`security/rules/${id}`, { method: "DELETE" }),
  ipLogs: (limit = 200) => request<RpIpLog[]>(`security/logs?limit=${limit}`),
  clearIpLogs: () => request<{ ok: boolean }>("security/logs/clear", { method: "POST" }),

  oauthStart: (name?: string, realm?: RpRealm, region?: string) =>
    request<{ id: string; url: string; expires_at: number; realm?: string; region?: string }>(
      "oauth/start", { method: "POST", body: { name, realm, region } }),
  oauthPoll: (id: string) => request<{ status: string; updated?: boolean; message?: string }>(`oauth/${id}/poll`, { method: "POST" }),
  oauthCancel: (id: string) => request<{ ok: boolean }>(`oauth/${id}`, { method: "DELETE" }),

  /**
   * 批量生成登录码。
   *
   * 一次最多 50 张：服务端 `BATCH_SCAN_MAX` 与 `browser_login.MAX_FLOWS`（60）之间留了
   * 余量，留给单账号入口正在用的那一张。分批不给「自动继续」是故意的 —— 一次 50 张
   * 已经超出正常人能扫完的量，再多只会是一堆过期链接。
   *
   * `items` 里每一格都可能单独失败（上游抖动、超过同 owner 上限），失败格带 `error`，
   * 成功的照常可用。**不能因为第 40 张失败就把前 39 张作废** —— 用户可能已经扫了。
   */
  batchScan: (count: number, realm?: RpRealm, region?: string, prefix?: string) =>
    request<{
      items: RpBatchScanItem[];
      ready: number;
      count: number;
      realm: string;
      region?: string;
      ttl: number;
    }>("oauth/batch/start", { method: "POST", body: { count, realm, region, prefix } }),

  testModel: (model: string, prompt: string) =>
    request<{ ok: boolean; status?: number; answer?: string; error?: string; seconds: number; usage?: { total_tokens?: number } }>(
      "test", { method: "POST", body: { model, prompt } }),

  // 上游分组：把账号池切成互不干扰的子池，密钥只落在自己那一组里。
  groups: () => request<RpGroupsPayload>("groups"),  createGroup: (body: { name: string; description?: string; accounts?: string[]; routing?: string; enabled?: boolean }) =>
    request<{ ok: boolean; id: string; name: string } & { groups: RpGroup[] }>("groups", { method: "POST", body }),
  updateGroup: (id: string, body: Partial<Pick<RpGroup, "name" | "description" | "enabled" | "routing">> & { accounts?: string[] }) =>
    request<{ ok: boolean; groups: RpGroup[] }>(`groups/${id}`, { method: "PATCH", body }),
  deleteGroup: (id: string) =>
    request<{ ok: boolean; removed: number; unbound_keys: number; groups: RpGroup[] }>(`groups/${id}`, { method: "DELETE" }),

  // 出口线路：按账号绑命名代理。
  egress: () => request<RpEgressPayload>("egress"),
  createEgressLine: (body: { name: string; url: string; enabled?: boolean }) =>
    request<{ ok: boolean; id: string; name: string } & { lines: RpEgressLine[] }>("egress/lines", { method: "POST", body }),
  updateEgressLine: (id: string, body: Partial<Pick<RpEgressLine, "name" | "enabled">> & { url?: string }) =>
    request<{ ok: boolean; lines: RpEgressLine[] }>(`egress/lines/${id}`, { method: "PATCH", body }),
  deleteEgressLine: (id: string) =>
    request<{ ok: boolean; freed: number; lines: RpEgressLine[] }>(`egress/lines/${id}`, { method: "DELETE" }),
  /** lineId 传空串表示解绑，该账号回落到直连。 */
  assignEgress: (accountId: string, lineId: string) =>
    request<{ ok: boolean; lines: RpEgressLine[] }>("egress/assign", { method: "POST", body: { account_id: accountId, line_id: lineId } }),

  // ── 服务设置（两端共用同一套接口与字段名）──────────────────────────────
  /** 读全部设置。 */
  settings: () => request<RpServiceSettings>("settings"),
  /** 改设置。浅合并：只更新请求体里出现过的键。 */
  saveSettings: (body: RpSettingsPatch) => request<RpServiceSettings>("settings", { method: "PUT", body }),

  // ── 告警通知 ────────────────────────────────────────────────────────
  /** 读告警配置。只读角色也能看（口令只回掩码）。 */
  alerts: () => request<RpAlerts>("alerts"),
  /**
   * 改告警配置。浅合并 —— 只传要改的字段。
   *
   * **口令字段有坑**：服务端只回掩码，所以表单里那个值可能是掩码而不是真口令。
   * 如果原样提交，就会把真口令覆盖成掩码字符。调用方必须在值是掩码时
   * 把这个键去掉（见 `pickAlertPatch`），不要在这里补救 ——
   * 这里看不出"用户是没改"还是"用户真想设成这个值"。
   */
  saveAlerts: (body: Partial<RpAlerts>) => request<RpAlerts>("alerts", { method: "PUT", body }),
  /**
   * 发一封测试邮件。
   *
   * 这是配置类功能唯一可靠的自证方式 —— 否则只能等下一次真告警才知道
   * "原来没配通"，而那时候问题已经发生了。
   */
  testAlerts: () => request<{ ok: boolean; message: string }>("alerts/test", { method: "POST" }),

  // ── 模型别名 ────────────────────────────────────────────────────────
  modelAliases: () => request<RpModelAliases>("model-aliases"),
  /**
   * **整表替换**别名映射，传空对象即清空。
   *
   * 后端刻意做成整表替换而不是逐条增删：别名表很小（十几条），整表提交能
   * 避开"先删后加"的中间态被读到的竞态。
   */
  saveModelAliases: (aliases: Record<string, string>) =>
    request<{ ok: boolean; aliases: Record<string, string> }>("model-aliases", { method: "POST", body: { aliases } }),

  // ── 模型目录 ────────────────────────────────────────────────────────
  /** 完整模型目录（含 tags / free）。模型白名单选择器的免费筛选靠它。 */
  modelCatalog: () => request<RpModelCatalog>("models"),

  // ── 用量统计 ────────────────────────────────────────────────────────
  usageStats: (days = 30, hours = 24) =>
    request<RpUsageStats>(`usage/stats?days=${days}&hours=${hours}`),

  // ── 任务流水 ────────────────────────────────────────────────────────
  /** 按类别汇总（成功/失败/成功率/最近一次）。与明细接口口径不同，不能合并。 */
  taskSummary: (hours = 24) =>
    request<{ hours: number; kinds: RpTaskSummaryRow[] }>(`pool/checkin-logs/summary?hours=${hours}`),
};

export type RpTaskSummaryRow = {
  kind: string;
  total: number;
  ok: number;
  failed: number;
  /** 最近一次的 unix 秒；这一类从没跑过就是 null。 */
  last: number | null;
  /** 成功数 / 总数。总数为 0 时是 0.0（不是 1.0 —— "没跑过"不等于"全成功"）。 */
  success_rate: number;
};

/** 请求日志里 outcome 字段的中文与色调。 */
export const OUTCOME_LABELS: Record<string, { label: string; tone: "ok" | "warn" | "bad" | "muted" }> = {
  success: { label: "成功", tone: "ok" },
  upstream_error: { label: "上游报错", tone: "bad" },
  no_account: { label: "无可用账号", tone: "warn" },
  rejected: { label: "被规则拒绝", tone: "warn" },
};

export const POOL_STATE_LABELS: Record<string, { label: string; tone: "ok" | "warn" | "bad" | "muted" }> = {
  available: { label: "可用", tone: "ok" },
  cooling: { label: "冷却中", tone: "warn" },
  paused: { label: "已暂停", tone: "muted" },
  exhausted: { label: "积分耗尽", tone: "warn" },
  invalid: { label: "凭据异常", tone: "bad" },
  missing: { label: "缺失", tone: "bad" },
};

/**
 * 厘 → 积分。只在显示层用，且固定 3 位小数（1 积分 = 1000 厘）。
 *
 * 服务端已经算好 balance_credit，正常情况下直接用那个字段；
 * 这个函数是给「流水里的 milli」这类只有厘的地方兜底用的。
 * 用整数除法 + 定点格式化，不做浮点累加 —— 本项目被 banker's rounding 坑过。
 */
export function creditFromMilli(milli: number | null | undefined): number {
  const value = Number(milli ?? 0);
  if (!Number.isFinite(value)) return 0;
  return Math.round(value) / 1000;
}

/** 积分显示：统一 3 位小数、去掉无意义的尾零（100 → "100"、1234 → "1.234"）。 */
export function fmtCredit(credit: number | null | undefined): string {
  const value = Number(credit ?? 0);
  if (!Number.isFinite(value)) return "0";
  const fixed = (Math.round(value * 1000) / 1000).toFixed(3);
  return fixed.replace(/\.?0+$/, "") || "0";
}

/**
 * 积分流水的时间格式：本地时区 YYYY-MM-DD HH:mm（sync-spec「显示口径」）。
 * 与 rp-shared 的 fmtTime 不同 —— 那个按秒、带秒且用 toLocaleString 的完整格式；
 * 流水表列多，这里要的是固定宽度。
 */
export function fmtCreditTime(seconds: number | null | undefined): string {
  const value = Number(seconds ?? 0);
  if (!value) return "—";
  const date = new Date(value * 1000);
  if (Number.isNaN(date.getTime())) return "—";
  const pad = (n: number) => String(n).padStart(2, "0");
  return date.getFullYear() + "-" + pad(date.getMonth() + 1) + "-" + pad(date.getDate())
    + " " + pad(date.getHours()) + ":" + pad(date.getMinutes());
}

export const ROUTING_LABELS: Record<string, { label: string; desc: string }> = {
  round_robin: {
    label: "轮流使用",
    desc: "每个请求依次落到不同的可用账号上。多个号分摊调用量，单号被高频使用的风险最低。",
  },
  manual: {
    label: "只用当前账号",
    desc: "所有请求都走「当前账号」这一个号，不会自动切到其它号。账号不可用时直接报错，不会偷换。",
  },
  least_used: {
    label: "最少使用优先",
    desc: "每次挑累计请求数最少的可用账号，让各号的用量尽量拉平。",
  },
};

/**
 * 数据面 base（展示用）。改由调用方传入 `proxy_service_status` 的 baseUrl——
 * 管理面通道与地址无关，这两个函数只服务「给外部 CLI 看的地址」展示。
 */
export function apiBaseFromService(baseUrl: string | null | undefined): string {
  return (baseUrl ?? "").replace(/\/+$/, "");
}

export function modelsEndpoint(baseUrl: string | null | undefined): string {
  return apiBaseFromService(baseUrl) + "/v1";
}
