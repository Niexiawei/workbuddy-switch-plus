/**
 * 「反代服务old」（Python）专用的管理面 HTTP 客户端。
 *
 * 管理面已不在 Rust 侧的 HTTP 上（进程内通道，见附录 C），而 old 页的三个面板
 * （管理令牌 / 后台账号 / 审计日志）依赖的 `/admin/api/tokens|users|audit-logs`
 * 只有 Python 服务实现。它们随 old 存活：这里用独立的 fetch（带会话令牌头）直连
 * Python 的地址（`reverse-proxy-service.ts` 采纳的 `adoptBaseUrl` 结果）。
 *
 * 会话令牌仍由 old 页面经 `rp-legacy` 登录获得——本模块只做端点级封装。
 */

/** 与 legacy/ReverseProxyPage 共享的模块级会话状态（登录成功后写入）。 */
let legacyToken = "";
let legacyBase = "http://127.0.0.1:8788";

export function setLegacySession(token: string | null, base?: string): void {
  legacyToken = token ?? "";
  if (base) legacyBase = base.replace(/\/+$/, "");
}

export function legacyBaseUrl(): string {
  return legacyBase;
}

export function hasLegacySession(): boolean {
  return legacyToken !== "";
}

export class LegacyError extends Error {
  status: number;
  constructor(message: string, status = 0) {
    super(message);
    this.name = "LegacyError";
    this.status = status;
  }
}

/** 发一次 Python 管理面请求（`/admin/api/*`）。 */
export async function legacyRequest<T>(path: string, options: { method?: string; body?: unknown } = {}): Promise<T> {
  const method = options.method ?? "GET";
  const url = `${legacyBase}/admin/api/${path.replace(/^\/+/, "")}`;
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (legacyToken) headers["X-Admin-Token"] = legacyToken;
  let response: Response;
  try {
    response = await fetch(url, { method, headers, credentials: "omit", body: options.body === undefined ? undefined : JSON.stringify(options.body) });
  } catch {
    throw new LegacyError("无法连接反代服务old，请确认它已启动", 0);
  }
  let data: unknown = null;
  try {
    data = await response.json();
  } catch {
    if (!response.ok) throw new LegacyError(`服务返回异常（HTTP ${response.status}）`, response.status);
    return undefined as T;
  }
  if (!response.ok) {
    const detail = (data as { detail?: unknown })?.detail;
    const message = typeof detail === "string" ? detail : `请求失败（HTTP ${response.status}）`;
    throw new LegacyError(message, response.status);
  }
  return data as T;
}
