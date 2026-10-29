import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ALERT_PASSWORD_MASK, pickAlertPatch, rp, setHostProvider } from "./reverse-proxy";

/**
 * 新增面板的 API 客户端契约：告警 / 模型别名 / 用量统计。
 *
 * 断言的是「打到哪个 URL、用什么方法、body 里有什么」—— 前端与后端之间
 * 最容易被静默改坏的那层。字段名写错前端不会报错，只会静静地什么都不做。
 *
 * 重点在**告警口令的掩码陷阱**：服务端只回 `********`，如果原样提交回去，
 * 真口令就被覆盖成这串星号，而接口照样返回 200、界面照样显示保存成功。
 * 之后所有告警邮件都发不出去，且没人会知道为什么。这条必须钉死。
 */

const TOKEN = "wbt_panel_token";

function stubLocalStorage() {
  const store = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => (store.has(key) ? (store.get(key) as string) : null),
    setItem: (key: string, value: string) => void store.set(key, String(value)),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
  return store;
}

function stubFetch(payload: unknown = { ok: true }) {
  const fetchMock = vi.fn((_url: string, _init?: RequestInit) =>
    Promise.resolve(
      new Response(JSON.stringify(payload), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    ),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

function call(fetchMock: ReturnType<typeof stubFetch>, index = 0): [string, RequestInit] {
  return fetchMock.mock.calls[index] as unknown as [string, RequestInit];
}

beforeEach(() => {
  stubLocalStorage();
// 地址与令牌都由桌面端注入（见 reverse-proxy.ts 的 setHostProvider）：
    // localStorage 里的旧键已退役，写它们不会生效，只会让断言静默通过。
    setHostProvider({ base: () => "http://127.0.0.1:8787", token: () => TOKEN });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("告警配置 API", () => {
  it("alerts() 打到 /admin/api/alerts，方法 GET", async () => {
    const fetchMock = stubFetch({ enabled: false });
    await rp.alerts();
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/alerts");
    expect(init.method ?? "GET").toBe("GET");
    expect(init.headers).toMatchObject({ "X-Proxy-Host-Token": TOKEN });
  });

  it("saveAlerts() 用 PUT 且只发传进来的键", async () => {
    const fetchMock = stubFetch({ enabled: true });
    await rp.saveAlerts({ credits_threshold: 555 });
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/alerts");
    expect(init.method).toBe("PUT");
    // 浅合并语义：没传的键绝不能出现在 body 里，否则会把另一端刚改的值覆盖回去。
    expect(JSON.parse(String(init.body))).toEqual({ credits_threshold: 555 });
  });

  it("testAlerts() 用 POST 打到 alerts/test", async () => {
    const fetchMock = stubFetch({ ok: true, message: "已发送" });
    const res = await rp.testAlerts();
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/alerts/test");
    expect(init.method).toBe("POST");
    expect(res.message).toBe("已发送");
  });
});

describe("告警口令掩码（最关键的一组）", () => {
  it("pickAlertPatch 会剔掉掩码口令", () => {
    const patch = pickAlertPatch({ password: ALERT_PASSWORD_MASK, credits_threshold: 10 });
    expect("password" in patch).toBe(false);
    expect(patch.credits_threshold).toBe(10);
  });

  it("pickAlertPatch 保留真实新口令（用户确实改了）", () => {
    const patch = pickAlertPatch({ password: "brand-new-secret" });
    expect(patch.password).toBe("brand-new-secret");
  });

  it("pickAlertPatch 保留空串（用户主动清空口令）", () => {
    // 空串和掩码是两件事：掩码=「没看到真口令」，空串=「明确要清掉」。
    // 把空串也剔掉的话，用户就永远清不掉已配置的口令。
    const patch = pickAlertPatch({ password: "" });
    expect("password" in patch).toBe(true);
    expect(patch.password).toBe("");
  });

  it("pickAlertPatch 剔掉只读字段 password_set", () => {
    const patch = pickAlertPatch({ password_set: true, enabled: true });
    expect("password_set" in patch).toBe(false);
    expect(patch.enabled).toBe(true);
  });

  it("掩码常量与服务端一致（********）", () => {
    // 这个字符串两边必须一模一样。写错了不会报错，只会静默覆盖真口令。
    expect(ALERT_PASSWORD_MASK).toBe("********");
    expect(ALERT_PASSWORD_MASK).toHaveLength(8);
  });

  it("模拟完整往返：载入→只改阈值→提交，口令不出现在 body 里", async () => {
    // 服务端回的就是掩码（这是它的既定行为，不是编的）
    const fromServer = {
      enabled: true, to: "a@b.c", host: "smtp.example.com", port: 465,
      username: "u", password: ALERT_PASSWORD_MASK, password_set: true,
      sender: "", tls: "ssl", credits_threshold: 100,
    };
    const fetchMock = stubFetch(fromServer);
    const cfg = await rp.alerts();
    expect(cfg.password).toBe(ALERT_PASSWORD_MASK);

    const body = pickAlertPatch({ ...cfg, credits_threshold: 200 });
    await rp.saveAlerts(body);

    const sent = JSON.parse(String(call(fetchMock, 1)[1].body));
    expect("password" in sent).toBe(false);
    expect(sent.credits_threshold).toBe(200);
  });

  it("反例：不经过 pickAlertPatch 就会把掩码当新口令提交", async () => {
    // 这条不是测产品逻辑，是把「为什么必须有 pickAlertPatch」固定下来。
    const fetchMock = stubFetch({ ok: true });
    await rp.saveAlerts({ password: ALERT_PASSWORD_MASK });
    const sent = JSON.parse(String(call(fetchMock)[1].body));
    expect(sent.password).toBe(ALERT_PASSWORD_MASK);
    // 于是服务端会把真口令存成 ******** —— 静默、无报错。
  });
});

describe("模型别名 API", () => {
  it("modelAliases() 用 GET 打到 model-aliases", async () => {
    const fetchMock = stubFetch({ aliases: {}, known_models: [], default_model: "x", unknown_targets: [] });
    await rp.modelAliases();
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/model-aliases");
    expect(init.method ?? "GET").toBe("GET");
  });

  it("saveModelAliases() 用 POST 且包在 aliases 键里", async () => {
    const fetchMock = stubFetch({ ok: true, aliases: {} });
    await rp.saveModelAliases({ "gpt-4o": "glm-5.2" });
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/model-aliases");
    expect(init.method).toBe("POST");
    // 后端读的是 body.aliases，不是裸对象 —— 少包一层会得到 400「aliases 必须是对象」。
    expect(JSON.parse(String(init.body))).toEqual({ aliases: { "gpt-4o": "glm-5.2" } });
  });

  it("传空表就是清空（整表替换语义）", async () => {
    const fetchMock = stubFetch({ ok: true, aliases: {} });
    await rp.saveModelAliases({});
    expect(JSON.parse(String(call(fetchMock)[1].body))).toEqual({ aliases: {} });
  });
});

describe("用量统计 API", () => {
  it("usageStats() 把 days/hours 拼进查询串", async () => {
    const fetchMock = stubFetch({ daily: [], hourly: [], totals: {}, window: {} });
    await rp.usageStats(7, 12);
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/usage/stats?days=7&hours=12");
    expect(init.method ?? "GET").toBe("GET");
  });

  it("usageStats() 默认 30 天 / 24 小时", async () => {
    const fetchMock = stubFetch({ daily: [], hourly: [], totals: {}, window: {} });
    await rp.usageStats();
    const [url] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/usage/stats?days=30&hours=24");
  });

  it("解析出后端返回的分桶与总量字段", async () => {
    stubFetch({
      daily: [{ date: "2026-10-14", calls: 10, failed: 1, success: 9, failure_rate: 0.1,
                tokens: 1000, credit: 1.25, tokens_avg: 100, duration_ms: 5000,
                duration_ms_avg: 500, first_token_ms: 900, first_token_ms_avg: 100 }],
      hourly: [],
      totals: { daily: { calls: 10, failed: 1, success: 9, failure_rate: 0.1, tokens: 1000,
                         credit: 1.25, tokens_avg: 100, duration_ms_avg: 500 }, hourly: {} },
      window: { days: 30, hours: 24, daily_keep: 60, hourly_keep: 48 },
    });
    const s = await rp.usageStats();
    expect(s.daily[0].date).toBe("2026-10-14");
    expect(s.daily[0].duration_ms_avg).toBe(500);
    expect(s.daily[0].first_token_ms_avg).toBe(100);
    expect(s.totals.daily.failure_rate).toBe(0.1);
    expect(s.window.daily_keep).toBe(60);
  });
});
