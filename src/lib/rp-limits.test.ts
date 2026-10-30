import { describe, expect, it, vi, afterEach, beforeEach } from "vitest";

import { rp, type RpAccount, setAdminChannel } from "./reverse-proxy";

/**
 * 账号限速（流量整形）的前端契约。
 *
 * 钉两件事：
 *
 * 1. **URL 与整体语义**。`PUT accounts/{id}/limits` 是整体写，三项必发。
 *    漏发某一项时服务端把它读作「不动这一项」—— 界面看着保存成功，
 *    实际有一项没变。这类错误前端不会报，只能靠断言 body 抓住。
 * 2. **0 是合法值**。0 表示不限制。任何 `||` 兜底都会把 0 变成默认值，
 *    表现为「明明填了 0，保存后还是 5」，且没有任何提示。
 *
 * 角标逻辑单独测：它决定用户能否看出「这个号和别的号不一样」。
 * 全池统一时挂角标 = 没有信息量；该显示时没显示 = 用户以为限速没生效。
 */


function stubLocalStorage() {
  const store = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => (store.has(key) ? (store.get(key) as string) : null),
    setItem: (key: string, value: string) => void store.set(key, String(value)),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
}

function account(over: Partial<RpAccount> = {}): RpAccount {
  return {
    id: "acc-1", name: "测试号", state: "available",
    weight: 1, max_concurrency: 0, min_interval: 0,
    ...over,
  } as RpAccount;
}

describe("账号限速", () => {
  beforeEach(() => {
    stubLocalStorage();
    // 真实的存储键是 `wb.rp.*`（见 reverse-proxy.ts 的 BASE_KEY / TOKEN_KEY）。
    // 写错键的后果是请求静默不带令牌 —— 401 而不是报错，很容易被当成后端问题。
    // 地址与令牌都由桌面端注入（见 reverse-proxy.ts 的 setAdminChannel）：
    // localStorage 里的旧键已退役，写它们不会生效，只会让断言静默通过。
    setAdminChannel(async (method, path, body) => {
  channelCalls.push({ method, path, body });
  return { status: channelStatus, body: channelReply };
});
  });
  afterEach(() => vi.unstubAllGlobals());

  it("打到 accounts/{id}/limits，用 PUT", async () => {
    const calls = stubFetch();
    await rp.setAccountLimits("acc-9", { weight: 2, max_concurrency: 3, min_interval: 1.5 });
    expect(calls).toHaveLength(1);
    expect(calls[0].path).toContain("accounts/acc-9/limits");
    expect(calls[0].method).toBe("PUT");
  });

  it("三项都发，包括值为 0 的项", async () => {
    const calls = stubFetch();
    // 权重 0 = 不参与轮转；并发 0 = 不限制；间隔 0 = 不限速。
    // 三者都必须原样出现在 body 里，不能被任何 `|| default` 吃掉。
    await rp.setAccountLimits("acc-1", { weight: 0, max_concurrency: 0, min_interval: 0 });
    expect(calls[0].body).toEqual({ weight: 0, max_concurrency: 0, min_interval: 0 });
  });

  it("小数权重原样保留", async () => {
    const calls = stubFetch();
    await rp.setAccountLimits("acc-1", { weight: 0.5, max_concurrency: 0, min_interval: 0 });
    expect((calls[0].body as { weight: number }).weight).toBe(0.5);
  });

  it("进程内通道发出且只发一次", async () => {
    const calls = stubFetch();
    await rp.setAccountLimits("acc-1", { weight: 1, max_concurrency: 0, min_interval: 0 });
    expect(calls).toHaveLength(1);
  });
});

/**
 * 角标：从账号字段拼出一行摘要。这里复制的是
 * `src/components/reverse-proxy/accounts-panel.tsx` 里 `limitsBadge()` 的口径，
 * 两边必须一致 —— 组件里改了口径而这里没改，就意味着用户在界面上
 * 看到的摘要和实际生效的值不同。
 */
function limitsBadge(a: RpAccount): string {
  const weight = Number((a as { weight?: number }).weight ?? 1);
  const concurrency = Number((a as { max_concurrency?: number }).max_concurrency ?? 0);
  const gap = Number((a as { min_interval?: number }).min_interval ?? 0);
  const parts: string[] = [];
  if (weight !== 1) parts.push(weight === 0 ? "停转" : `${weight}x`);
  if (concurrency > 0) parts.push(`≤${concurrency}`);
  if (gap > 0) parts.push(`${gap}s`);
  return parts.join(" ");
}

describe("限速角标", () => {
  it("全默认时为空", () => {
    expect(limitsBadge(account())).toBe("");
  });

  it("字段缺失按默认处理，不报错", () => {
    expect(limitsBadge({ id: "x", name: "n", state: "available" } as unknown as RpAccount)).toBe("");
  });

  it("权重 0 显示「停转」而不是「0x」", () => {
    expect(limitsBadge(account({ weight: 0 }))).toBe("停转");
  });

  it("权重 2 显示 2x", () => {
    expect(limitsBadge(account({ weight: 2 }))).toBe("2x");
  });

  it("小数权重原样显示", () => {
    expect(limitsBadge(account({ weight: 0.5 }))).toBe("0.5x");
  });

  it("并发上限显示 ≤N", () => {
    expect(limitsBadge(account({ max_concurrency: 3 }))).toBe("≤3");
  });

  it("最小间隔显示 Ns", () => {
    expect(limitsBadge(account({ min_interval: 2.5 }))).toBe("2.5s");
  });

  it("三项齐上时按 权重/并发/间隔 顺序拼接", () => {
    expect(limitsBadge(account({ weight: 2, max_concurrency: 4, min_interval: 1 }))).toBe("2x ≤4 1s");
  });

  it("权重 1 不算偏离默认，不占位", () => {
    expect(limitsBadge(account({ weight: 1, max_concurrency: 2 }))).toBe("≤2");
  });
});
type ChannelCall = { method: string; path: string; body: unknown };

let channelCalls: ChannelCall[] = [];
let channelReply: unknown = { ok: true };
let channelStatus = 200;

function stubFetch(payload: unknown = { ok: true }) {
  channelCalls = [];
  channelReply = payload;
  channelStatus = 200;
  return channelCalls;
}

