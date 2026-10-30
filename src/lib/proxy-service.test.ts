import { describe, expect, it } from "vitest";

import { isValidPort, parsePort, portLine, type ProxyServiceStatus } from "./proxy-service";

/**
 * 内置服务端口与状态文案的前端契约。
 *
 * 这套逻辑的价值在于「用户能不能看出端口变了」：Rust 侧在 8787 被占时会自动
 * +1，如果界面还显示 8787，客户端就连不上却看不出原因。所以这里钉三件事：
 *   1. 端口输入的判定边界（0 / 65536 / 小数 / 空串都不该通过）；
 *   2. 状态条文案的单一来源（Rust 给了 `portNotice` 就用它）；
 *   3. 未运行 / 失败时文案不撒谎。
 */

function status(over: Partial<ProxyServiceStatus> = {}): ProxyServiceStatus {
  return {
    engine: "rust",
    running: true,
    phase: "running",
    desiredPort: 8787,
    port: 8787,
    portShifted: false,
    baseUrl: "http://127.0.0.1:8787",
    portNotice: null,
    ...over,
  };
}

describe("端口输入", () => {
  it("合法区间是 1–65535", () => {
    expect(isValidPort(1)).toBe(true);
    expect(isValidPort(8787)).toBe(true);
    expect(isValidPort(65535)).toBe(true);
    expect(isValidPort(0)).toBe(false);
    expect(isValidPort(65536)).toBe(false);
    expect(isValidPort(-1)).toBe(false);
  });

  it("小数与 NaN 不算合法端口", () => {
    expect(isValidPort(8787.5)).toBe(false);
    expect(isValidPort(Number.NaN)).toBe(false);
    expect(isValidPort(Number.POSITIVE_INFINITY)).toBe(false);
  });

  it("parsePort 允许首尾空格", () => {
    expect(parsePort("  9000  ")).toBe(9000);
  });

  it("parsePort 拒绝空串、纯字母、混入空格与带符号的数字", () => {
    expect(parsePort("")).toBeNull();
    expect(parsePort("   ")).toBeNull();
    expect(parsePort("port")).toBeNull();
    expect(parsePort("90 00")).toBeNull();
    expect(parsePort("+9000")).toBeNull();
    expect(parsePort("9000.5")).toBeNull();
  });

  it("parsePort 拒绝越界值", () => {
    expect(parsePort("0")).toBeNull();
    expect(parsePort("65536")).toBeNull();
    expect(parsePort("99999")).toBeNull();
  });
});

describe("状态条文案", () => {
  it("没有状态时说明还在读", () => {
    expect(portLine(null)).toBe("正在读取服务状态…");
  });

  it("停止时如实说停止", () => {
    expect(portLine(status({ running: false, phase: "stopped" }))).toBe("内置服务已停止");
  });

  it("失败时优先显示原因，而不是笼统一句失败", () => {
    const line = portLine(status({ running: false, phase: "failed", error: "端口 8787 起连续 20 个都被占用" }));
    expect(line).toBe("端口 8787 起连续 20 个都被占用");
  });

  it("失败但没有原因时给一句兜底", () => {
    expect(portLine(status({ running: false, phase: "failed" }))).toBe("内置服务启动失败");
  });

  it("端口偏移时用后端给的文案（单一来源，不在前端重拼）", () => {
    const line = portLine(status({ portShifted: true, port: 8788, portNotice: "8787 已被占用，已在 8788 运行" }));
    expect(line).toBe("8787 已被占用，已在 8788 运行");
  });

  it("正常运行且未偏移时显示监听地址", () => {
    expect(portLine(status())).toBe("监听 http://127.0.0.1:8787");
  });

  it("没有 baseUrl 时退回用期望端口拼，不显示 undefined", () => {
    expect(portLine(status({ baseUrl: undefined }))).toBe("监听 127.0.0.1:8787");
  });
});
