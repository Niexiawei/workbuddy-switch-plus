import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { normalizeProxyBase } from "./cli-link";
import { rp, setAdminChannel } from "./reverse-proxy";

/**
 * CodeBuddy CLI 接入卡片的两类契约：
 *   1. 反代地址归一化——少了版本段 CLI 会打到门户根路径（405）；
 *   2. 卡片调用的接口地址/方法/鉴权头——写错前端不会报错，只会静默不生效。
 */

const PAGE_ORIGIN = "http://127.0.0.1:6100";

function stubLocalStorage() {
  const store = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => (store.has(key) ? (store.get(key) as string) : null),
    setItem: (key: string, value: string) => void store.set(key, String(value)),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
}

type ChannelCall = { method: string; path: string; body: unknown };  let channelCalls: ChannelCall[] = []; let channelReply: unknown = { ok: true }; let channelStatus = 200;  /** 装一个会记录调用的通道桩。payload 与旧 stubFetch 的语义一致。 */ function stubFetch(payload: unknown = { ok: true }) {   channelCalls = [];   channelReply = payload;   channelStatus = 200;   return channelCalls; }  /** 取第 index 次调用的 [path, method, body]。 */ function call(calls: ReturnType<typeof stubFetch>, index = 0): [string, string, unknown] {   const entry = calls[index];   if (!entry) throw new Error(`no call ${index}`);   return [entry.path, entry.method, entry.body]; }
beforeEach(() => {
  stubLocalStorage();
// 地址与令牌都由桌面端注入（见 reverse-proxy.ts 的 setAdminChannel）：
    // localStorage 里的旧键已退役，写它们不会生效，只会让断言静默通过。
    setAdminChannel(async (method, path, body) => {
  channelCalls.push({ method, path, body });
  return { status: channelStatus, body: channelReply };
});
  vi.stubGlobal("window", { location: { origin: PAGE_ORIGIN } });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("normalizeProxyBase", () => {
  it("bare 地址补 /v1", () => {
    expect(normalizeProxyBase("http://127.0.0.1:8787")).toBe("http://127.0.0.1:8787/v1");

  });

  it("已带版本段时只去空白与结尾斜杠", () => {
    expect(normalizeProxyBase("  http://127.0.0.1:8787/v1/  ")).toBe("http://127.0.0.1:8787/v1");
    expect(normalizeProxyBase("https://example.com/v2")).toBe("https://example.com/v2");
  });

  it("空串保持空串（由调用方禁用按钮，不编默认地址）", () => {
    expect(normalizeProxyBase("   ")).toBe("");
  });
});

// 原「开启/关闭反代模式的 webui 路由」两条已删除：它们测的是浏览器形态下打
// `/api/codebuddy-cli/proxy` 的 HTTP 路径，而该形态（wb-switch + httpCall + ROUTES）
// 已随 I6 整包移除。桌面端走 `invoke("enable_codebuddy_cli_proxy")`，由 Rust 侧覆盖。

describe("CLI 专用密钥的管理端契约", () => {
  it("cliKey() 是 GET /admin/api/cli/key", async () => {
    const fetchMock = stubFetch({ id: "k1", key: "sk-wb-1", created: true, realm: "cn" });
    const result = await rp.cliKey();
    const [path, method] = call(fetchMock);
    expect(path).toBe("cli/key");
    expect(method ?? "GET").toBe("GET");
    expect(result.key).toBe("sk-wb-1");
  });

  it("rotateCliKey() 是 POST .../cli/key/rotate", async () => {
    const fetchMock = stubFetch({ id: "k2", key: "sk-wb-2", created: true, realm: "cn" });
    await rp.rotateCliKey();
    const [path, method] = call(fetchMock);
    expect(path).toBe("cli/key/rotate");
    expect(method).toBe("POST");
  });

  it("revokeCliKey() 是 DELETE .../cli/key", async () => {
    const fetchMock = stubFetch({ ok: true, revoked: 1 });
    await rp.revokeCliKey();
    const [path, method] = call(fetchMock);
    expect(path).toBe("cli/key");
    expect(method).toBe("DELETE");
  });
});
