/**
 * 内置反代（Rust）的宿主接口。
 *
 * 与旧的「Python 服务启动器」(`reverse-proxy-service.ts`) 是两套东西：
 *   · 那份管独立 exe 的进程、端口探测、提权停止 —— 留给「反代服务old」；
 *   · 这份管**进程内**的 axum：随主程序启动，停机也能在这里拉起来。
 *
 * 前端拿到的字段刻意保持扁平（`port` / `desiredPort` / `portShifted` / `baseUrl`），
 * 因为这几项要直接显示在状态条里，多一层嵌套只会让组件多写解构。
 */

import { invoke } from "@tauri-apps/api/core";

/** `proxy_service_status` 的返回。`phase` 是给界面做分支用的粗粒度状态。 */
export type ProxyServiceStatus = {
  engine: "rust";
  running: boolean;
  phase: "stopped" | "starting" | "running" | "failed";
  /** 配置里的期望端口（用户填的）。 */
  desiredPort: number;
  /** 实际监听端口。未运行时缺省。 */
  port?: number;
  /** 实际端口与期望端口不一致（被占用后自动 +1）。 */
  portShifted: boolean;
  /** 形如 `http://127.0.0.1:8788`。未运行时缺省。 */
  baseUrl?: string;
  /** 端口偏移时的现成提示文案；未偏移时为 null。 */
  portNotice?: string | null;
  /** 宿主令牌（管理面鉴权用）。 */
  hostToken: string;
  /** `phase === "failed"` 时的原因。 */
  error?: string;
};

export async function proxyServiceStatus(): Promise<ProxyServiceStatus> {
  return invoke<ProxyServiceStatus>("proxy_service_status");
}

export async function proxyServiceStart(): Promise<ProxyServiceStatus> {
  return invoke<ProxyServiceStatus>("proxy_service_start");
}

export async function proxyServiceStop(): Promise<ProxyServiceStatus> {
  return invoke<ProxyServiceStatus>("proxy_service_stop");
}

/** 改端口：写入设置并立即重绑。端口非法时以 `Error` 抛出。 */
export async function proxyServiceSetPort(port: number): Promise<ProxyServiceStatus> {
  return invoke<ProxyServiceStatus>("proxy_service_set_port", { port });
}

/**
 * 取宿主令牌。
 *
 * 只存在进程内存里，不落 localStorage：页面刷新时重新 invoke 一次即可。
 * 桌面端以外的宿主没有这个命令，调用方应先判 `isDesktop()`。
 */
export async function proxyAdminToken(): Promise<string> {
  return invoke<string>("proxy_admin_token");
}

/** 端口合法区间。0 在 Rust 侧表示「系统分配」，对用户无意义，因此不接受。 */
export function isValidPort(value: number): boolean {
  return Number.isInteger(value) && value >= 1 && value <= 65535;
}

/**
 * 端口输入框的解析：返回 `null` 表示「还不能提交」。
 *
 * 允许首尾空格；不合法一律 null，由调用方禁用按钮——比提交后再报错快一步。
 */
export function parsePort(raw: string): number | null {
  const text = raw.trim();
  if (!/^\d+$/.test(text)) return null;
  const value = Number(text);
  return isValidPort(value) ? value : null;
}

/**
 * 状态条要显示的那一行文案。
 *
 * 单一来源在 Rust 侧（`portNotice`），这里只负责在未运行 / 失败时补一条。
 * 刻意不用 `portShifted` 自己拼中文：同一个提示说两遍、改两处，迟早对不上。
 */
export function portLine(status: ProxyServiceStatus | null): string {
  if (!status) return "正在读取服务状态…";
  if (status.phase === "failed") return status.error ?? "内置服务启动失败";
  if (!status.running) return "内置服务已停止";
  if (status.portNotice) return status.portNotice;
  return `监听 ${status.baseUrl ?? `127.0.0.1:${status.desiredPort}`}`;
}
