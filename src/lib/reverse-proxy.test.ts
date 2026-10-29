import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  creditFromMilli, fmtCredit, fmtCreditTime, rp, setHostProvider, type RpAccount,
} from "./reverse-proxy";
import { activeNote, fmtLeft, upstreamNote } from "@/components/reverse-proxy/accounts-panel";

/**
 * 活跃上报的 API 客户端契约。
 *
 * 这几条断言的是「打到哪个 URL、用什么方法、带不带鉴权头、body 里有没有那三个新字段」——
 * 也就是前端与后端接口之间那层最容易被静默改坏的东西。服务端字段名写错时
 * 前端不会报错，只会静静地什么都不做，所以这里逐字钉住路径与字段名。
 *
 * node 环境没有 localStorage，而 reverse-proxy.ts 的 read/write 会直接碰它，
 * 因此下面 stubGlobal 一个最小实现（get/set/removeItem + clear）。
 */

const TOKEN = "wbt_verify_token";

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

/** 取第 index 次 fetch 调用的 [url, init]。 */
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

describe("活跃上报 API 客户端", () => {
  it("accountAction(active) 打到单账号动作路径，方法 POST 且带 X-Proxy-Host-Token", async () => {
    const fetchMock = stubFetch({ id: "a1", ok: false, sent: 0, count: 1, conversationId: "wb2api-1", message: "凭据无效" });

    await rp.accountAction("a1", "active");

    expect(fetchMock).toHaveBeenCalledTimes(1);
    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/accounts/a1/actions/active");
    expect(init.method).toBe("POST");
    expect(init.headers).toMatchObject({ "X-Proxy-Host-Token": TOKEN });
    // 动作接口不带 body，全部语义在路径里。
    expect(init.body).toBeUndefined();
  });

  it("accountAction(active) 的返回形状解出 sent / count / conversationId / streak", async () => {
    stubFetch({ id: "a1", ok: true, sent: 3, count: 3, conversationId: "wb2api-42", streak: 7, streak_ok: true, message: "已上报 3 条" });

    const result = await rp.accountAction("a1", "active");

    expect(result.sent).toBe(3);
    expect(result.count).toBe(3);
    expect(result.conversationId).toBe("wb2api-42");
    expect(result.streak).toBe(7);
    expect(result.streak_ok).toBe(true);
  });

  it("poolAction(active) 打到批量动作路径", async () => {
    const fetchMock = stubFetch({ results: [{ id: "a1", ok: true, message: "ok" }] });

    await rp.poolAction("active");

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/pool/actions/active");
    expect(init.method).toBe("POST");
  });

  it("poolSettings 是 PATCH，body 里三个活跃上报字段一个不少", async () => {
    const fetchMock = stubFetch({ ok: true, pool: {} });

    await rp.poolSettings({ auto_active: true, active_time: "10:00", active_reports: 3 });

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/pool/settings");
    expect(init.method).toBe("PATCH");
    // 逐字段比对：少发一个字段服务端会当「不改」，静默丢配置。
    expect(JSON.parse(String(init.body))).toEqual({ auto_active: true, active_time: "10:00", active_reports: 3 });
  });

  it("checkinLogs(100, active) 的 URL 带 kind=active", async () => {
    const fetchMock = stubFetch([]);

    await rp.checkinLogs(100, "active");

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/pool/checkin-logs?limit=100&kind=active");
    expect(init.method).toBe("GET");
  });

  it("clearCheckinLogs(active) 按类别清空，只清 active", async () => {
    const fetchMock = stubFetch({ ok: true, removed: 2 });

    await rp.clearCheckinLogs("active");

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/pool/checkin-logs/clear?kind=active");
    expect(init.method).toBe("POST");
  });

  it("不带 kind 时按类别清空的 URL 不带查询串（清全部）", async () => {
    const fetchMock = stubFetch({ ok: true, removed: 9 });

    await rp.clearCheckinLogs();

    expect(call(fetchMock)[0]).toBe("http://127.0.0.1:8787/admin/api/pool/checkin-logs/clear");
  });

  it("服务端 400 时抛 RpError 并带上 detail", async () => {
    const fetchMock = vi.fn(() =>
      Promise.resolve(
        new Response(JSON.stringify({ detail: "active_reports 必须是 1-5 的整数" }), {
          status: 400,
          headers: { "Content-Type": "application/json" },
        }),
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    await expect(rp.poolSettings({ active_reports: 9 })).rejects.toMatchObject({
      status: 400,
      message: "active_reports 必须是 1-5 的整数",
    });
  });
});
/**
 * 账号卡片上「活跃上报」那一行小字的三档语义。
 *
 * 这一行是本功能唯一一处把「报上去了」与「真的生效了」摆给用户看的地方，
 * 三档分不开的话，最关键的失败形态（报了 200 但事件被静默丢弃）在界面上
 * 就完全不可见——用户只会看到一个永远点不亮的连登。
 */
/** 只关心活跃上报那几个字段，其余必填项给一组无意义的占位值。 */
const account = (fields: Partial<RpAccount>): RpAccount => ({
  id: "a1", name: "账号", enabled: true, active: true, ...fields,
});

describe("活跃上报状态小字", () => {
  it("确认生效时显示已上报，并带上连登天数", () => {
    expect(activeNote(account({ active_today: true, active_streak: 5 })))
      .toBe("今日已活跃上报 · 连登 5 天");
  });

  it("生效了但回读天数为 0 时不显示天数（0 不是连登天数）", () => {
    // active_today 与 active_streak 同时存在但天数为 0 是矛盾的，服务端不会这么发；
    // 这里守的是「别把 0 当天数渲染成『连登 0 天』」。
    expect(activeNote(account({ active_today: true, active_streak: 0 }))).toBe("今日已活跃上报");
    expect(activeNote(account({ active_today: true, active_streak: null }))).toBe("今日已活跃上报");
  });

  it("终局跳过（国际版账号）说「今日跳过」，不说「待确认」", () => {
    // 这类账号今天不会再被尝试，显示成待重试会让人一直等一个不会发生的动作。
    expect(activeNote(account({ active_today: false, active_skip: "intl" }))).toBe("今日跳过活跃上报");
  });

  it("报了但没证明生效时说「待确认」——这是静默丢弃唯一可见的地方", () => {
    expect(activeNote(account({ active_today: false, active_error: "已上报，但连登天数仍为 0（可能被上游丢弃）" })))
      .toBe("活跃上报待确认");
  });

  it("今天压根没轮到它时不显示任何东西", () => {
    expect(activeNote(account({}))).toBeNull();
    expect(activeNote(account({ active_date: "2020-01-01", active_today: false }))).toBeNull();
  });

  it("已生效时优先显示生效，不被陈旧的 active_error 盖掉", () => {
    // 服务端成功会把 active_error 清空；万一拿到旧数据，也不能让旧报错盖住成功。
    expect(activeNote(account({ active_today: true, active_streak: 3, active_error: "旧报错" })))
      .toBe("今日已活跃上报 · 连登 3 天");
  });
});

describe("剩余时长文案", () => {
  it("按量级换单位，不把秒数直接摆出来", () => {
    expect(fmtLeft(45)).toBe("45 秒");
    expect(fmtLeft(270)).toBe("4 分 30 秒");
    expect(fmtLeft(300)).toBe("5 分");
    expect(fmtLeft(7500)).toBe("2 小时 5 分");
    expect(fmtLeft(7200)).toBe("2 小时");
    expect(fmtLeft(3 * 86400)).toBe("3 天");
  });

  it("没有剩余时间就返回空串", () => {
    // null 表示「上游没给 / 已经过期」，不是「0 秒」——两者都该什么都不显示。
    for (const value of [null, undefined, 0, -1, NaN]) {
      expect(fmtLeft(value as number | null)).toBe("");
    }
  });
});

describe("上游冷却小字（模型级）", () => {
  it("读不到上游时完全不显示——「未知」不能画成「没有冷却」", () => {
    // up_cooling 为 null/undefined 是「没配或读不到」。画成「无冷却」是在替上游
    // 下结论，比不显示更糟：用户会以为这号健康。
    expect(upstreamNote(account({}))).toBeNull();
    expect(upstreamNote(account({ up_cooling: null }))).toBeNull();
  });

  it("冷却中时给出剩余时间与逐个模型的恢复时长", () => {
    const note = upstreamNote(account({
      up_cooling: true, up_cool_remaining: 270,
      up_models: [
        { model: "glm-5.3", remaining_sec: 900, reason: "6004 model rate limit" },
        { model: "kimi-k3-1", remaining_sec: 60, reason: "6004 model rate limit" },
      ],
    }));
    expect(note).toBe("上游冷却中 · 还要 4 分 30 秒 · 限流模型：glm-5.3（15 分）、kimi-k3-1（1 分）");
  });

  it("区分「被限流」与「这个号没开通该模型」——处置方式相反", () => {
    // 6004 等一会儿就好；11102 等多久都不会好、该换模型。混成一句会误导。
    const note = upstreamNote(account({
      up_cooling: true,
      up_models: [
        { model: "glm-5.3", remaining_sec: 600, reason: "6004 model rate limit" },
        { model: "claude-sonnet-4-5", reason: "11102 model not available" },
      ],
    }));
    expect(note).toContain("限流模型：glm-5.3（10 分）");
    expect(note).toContain("未开通模型：claude-sonnet-4-5");
  });

  it("只说账号冷却、没有模型台账时也能显示", () => {
    expect(upstreamNote(account({ up_cooling: true, up_cool_remaining: 30 })))
      .toBe("上游冷却中 · 还要 30 秒");
  });

  it("冷却中但上游没给剩余秒数时不硬编一个时长", () => {
    expect(upstreamNote(account({ up_cooling: true, up_cool_remaining: null })))
      .toBe("上游冷却中");
  });

  it("连败降权单独说明——它与限流退避都显示「冷却中」，但一个该查", () => {
    const future = Math.floor(Date.now() / 1000) + 600;
    const note = upstreamNote(account({
      up_cooling: true, up_consecutive_fails: 6, up_degrade_until: future,
    }));
    expect(note).toContain("连败降权 6 次");
  });

  it("降权已过期就不再提", () => {
    const past = Math.floor(Date.now() / 1000) - 600;
    expect(upstreamNote(account({ up_cooling: true, up_degrade_until: past })))
      .toBe("上游冷却中");
  });

  it("脏数据不崩：up_models 不是数组、元素缺 model", () => {
    expect(upstreamNote(account({ up_cooling: true, up_models: undefined })))
      .toBe("上游冷却中");
    expect(upstreamNote(account({
      up_cooling: true,
      up_models: [{ remaining_sec: 60 }, { model: "ok", remaining_sec: 60 }] as never,
    }))).toBe("上游冷却中 · 限流模型：ok（1 分）");
  });
});


/** 服务设置接口的客户端契约（两端共用同一套字段名）。 */
describe("服务设置 API 客户端", () => {
  it("settings 是 GET /admin/api/settings", async () => {
    const fetchMock = stubFetch({ site_name: "WB" });

    await rp.settings();

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/settings");
    expect(init.method).toBe("GET");
  });

  it("saveSettings 是 PUT，且只发传进来的键（浅合并，不能整份回传）", async () => {
    const fetchMock = stubFetch({ site_name: "WB" });

    await rp.saveSettings({ site_name: "工作台" });

    const [url, init] = call(fetchMock);
    expect(url).toBe("http://127.0.0.1:8787/admin/api/settings");
    expect(init.method).toBe("PUT");
    // 浅合并：只发改动过的键。整份回传会让两端互相覆盖。
    expect(JSON.parse(String(init.body))).toEqual({ site_name: "工作台" });
  });
});


/**
 * 显示口径（sync-spec「显示口径」）。
 *
 * 这些是两端必须一模一样的换算与措辞：
 * 网页端与桌面端只要有一边算错，同一个用户的余额就会显示成两个数。
 */
describe("积分显示口径", () => {
  it("厘 → 积分：1000 厘 = 1 积分", () => {
    expect(fmtCredit(creditFromMilli(1000))).toBe("1");
    expect(fmtCredit(creditFromMilli(1))).toBe("0.001");
    expect(fmtCredit(creditFromMilli(1234))).toBe("1.234");
  });

  it("整数积分不显示多余的 .000", () => {
    expect(fmtCredit(100)).toBe("100");
    expect(fmtCredit(0)).toBe("0");
  });

  it("负余额（欠费）照常显示符号", () => {
    expect(fmtCredit(-50)).toBe("-50");
  });

  it("脏数据不崩：null / undefined / NaN 都算 0", () => {
    expect(fmtCredit(null)).toBe("0");
    expect(fmtCredit(undefined)).toBe("0");
    expect(fmtCredit(Number.NaN)).toBe("0");
  });


  it("流水时间用本地时区的 YYYY-MM-DD HH:mm", () => {
    // 用本地时间构造，避免断言依赖运行机器的时区偏移。
    const local = new Date(2024, 4, 6, 9, 7, 0).getTime() / 1000;
    expect(fmtCreditTime(local)).toBe("2024-05-06 09:07");
  });

  it("时间缺值显示 —，而不是 1970", () => {
    expect(fmtCreditTime(0)).toBe("—");
    expect(fmtCreditTime(null)).toBe("—");
  });


});
