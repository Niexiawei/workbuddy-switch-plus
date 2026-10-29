import { describe, expect, it, vi, afterEach, beforeEach } from "vitest";

import { rp, setHostProvider } from "./reverse-proxy";
import { logTps } from "@/components/reverse-proxy/request-log-panel";

/**
 * 0.1.3 前端契约：输出速率、任务汇总、模型目录、审计日志。
 *
 * 这四条都有同一个隐患——**算错不会报错，只会安静地给出一个看起来合理的数字**。
 * 尤其 tok/s：分母用错（把排队和鉴权时间也算进生成时间）会得到偏低的速率，
 * 用户据此换模型，而实际上问题在网络。所以这里钉的不是"能跑通"，是口径。
 */

const TOKEN = "wbt_013_token";

function stubLocalStorage() {
  const store = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => (store.has(key) ? (store.get(key) as string) : null),
    setItem: (key: string, value: string) => void store.set(key, String(value)),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
}

type Call = { url: string; method: string; body: unknown; headers: Headers };

function stubFetch(calls: Call[], reply: unknown = { ok: true }) {
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    calls.push({
      url: String(url),
      method: String(init?.method ?? "GET"),
      body: init?.body ? JSON.parse(String(init.body)) : undefined,
      headers: new Headers(init?.headers),
    });
    return new Response(JSON.stringify(reply), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  });
}

beforeEach(() => {
  stubLocalStorage();
// 地址与令牌都由桌面端注入（见 reverse-proxy.ts 的 setHostProvider）：
    // localStorage 里的旧键已退役，写它们不会生效，只会让断言静默通过。
    setHostProvider({ base: () => "http://127.0.0.1:8787", token: () => TOKEN });
});
afterEach(() => vi.unstubAllGlobals());

// ── 输出速率 ────────────────────────────────────────────────────────

describe("单条请求的 tok/s", () => {
  it("流式：分母是 总耗时 − 首字耗时", () => {
    // 1000 token / (5000 - 1000) ms = 1000 / 4 s = 250
    expect(logTps({ ok: true, tokens: 1000, duration_ms: 5000, first_token_ms: 1000 })).toBe("250.0");
  });

  it("非流式：没有首字时间，用整段耗时", () => {
    expect(logTps({ ok: true, tokens: 100, duration_ms: 2000 })).toBe("50.0");
  });

  it("失败请求不给速率", () => {
    // 重试会把排队与鉴权时间摊进耗时，混进去速率会被压成噪声。
    expect(logTps({ ok: false, tokens: 1000, duration_ms: 5000, first_token_ms: 1000 })).toBe("—");
  });

  it("0 token 不给速率", () => {
    expect(logTps({ ok: true, tokens: 0, duration_ms: 5000 })).toBe("—");
  });

  it("首字晚于总耗时不产生负数", () => {
    // 时钟回拨 / 统计交叉时会拿到这种数据，宁可显示 — 也不能显示 -250.0。
    expect(logTps({ ok: true, tokens: 100, duration_ms: 1000, first_token_ms: 1500 })).toBe("—");
  });

  it("首字正好等于总耗时不给速率", () => {
    expect(logTps({ ok: true, tokens: 100, duration_ms: 1000, first_token_ms: 1000 })).toBe("—");
  });

  it("字段缺失不抛异常", () => {
    expect(logTps({} as { ok?: boolean })).toBe("—");
  });
});

// ── API 契约 ────────────────────────────────────────────────────────

describe("任务汇总", () => {
  it("打到 pool/checkin-logs/summary，带 hours", async () => {
    const calls: Call[] = [];
    stubFetch(calls, { hours: 24, kinds: [] });
    await rp.taskSummary(24);
    expect(calls[0].url).toContain("pool/checkin-logs/summary?hours=24");
    expect(calls[0].method).toBe("GET");
  });

  it("与明细接口是两个不同端点", async () => {
    // 合并成一个会在"最近 100 条"和"24 小时计数"之间产生口径歧义。
    expect(rp.taskSummary.toString()).not.toContain("checkin-logs?");
  });
});

describe("模型目录", () => {
  it("打到 models，用 GET", async () => {
    const calls: Call[] = [];
    stubFetch(calls, { models: [], total: 0, default_model: "", aliases: {}, source: "test" });
    await rp.modelCatalog();
    expect(calls[0].url).toContain("/models");
    expect(calls[0].method).toBe("GET");
    // 不能是 model-aliases —— 那是另一个端点，回的结构完全不一样。
    expect(calls[0].url).not.toContain("model-aliases");
  });
});

describe("审计日志", () => {
  it("默认拉 200 条", async () => {
    const calls: Call[] = [];
    stubFetch(calls, []);
    await rp.auditLogs();
    expect(calls[0].url).toContain("audit-logs?limit=200");
  });

  it("清空用 POST", async () => {
    const calls: Call[] = [];
    stubFetch(calls, { ok: true, removed: 3 });
    await rp.clearAuditLogs();
    expect(calls[0].url).toContain("audit-logs/clear");
    expect(calls[0].method).toBe("POST");
  });

  it("带鉴权头", async () => {
    const calls: Call[] = [];
    stubFetch(calls, []);
    await rp.auditLogs();
    expect(calls[0].headers.get("X-Proxy-Host-Token")).toBe(TOKEN);
  });
});

describe("IP 访问控制", () => {
  it("访问日志字段口径是 ts / blocked，不是 time / allowed", async () => {
    // 这两个是两套历史实现，混用会让表格整列显示成「—」而不报错。
    const sample = [{ ts: 1, ip: "1.2.3.4", path: "/v1/chat", blocked: true, reason: "deny", ua: "x" }];
    const calls: Call[] = [];
    stubFetch(calls, sample);
    const rows = await rp.ipLogs();
    expect(calls[0].url).toContain("security/logs?limit=200");
    expect(rows[0]).toHaveProperty("ts");
    expect(rows[0]).toHaveProperty("blocked");
  });

  it("加规则用 POST，body 带 kind / cidr / note", async () => {
    const calls: Call[] = [];
    stubFetch(calls, { id: "r1", kind: "deny", cidr: "10.0.0.0/8", note: "", created_at: 0 });
    await rp.addIpRule("deny", "10.0.0.0/8", "公司出口");
    expect(calls[0].method).toBe("POST");
    expect(calls[0].body).toEqual({ kind: "deny", cidr: "10.0.0.0/8", note: "公司出口" });
  });
});
