import { invoke } from "@tauri-apps/api/core";

import { isDesktop } from "@/lib/api";
import { DEFAULT_BASE, getBaseUrl, setBaseUrl } from "@/lib/reverse-proxy";
import {
  serviceStart,
  serviceStatus,
  serviceStop,
  serviceSupported,
  type RpServiceStatus,
} from "@/lib/reverse-proxy-service";

/**
 * 反代引擎选择（灰度期）：Python 独立服务 vs Rust 进程内引擎。
 *
 * 选择存在 localStorage；切换时把管理面的 base URL 指向对应端口，
 * 页面重新加载即可对比两套实现。Rust 引擎由 Tauri 命令托管，
 * 没有独立进程/exe 的概念，所以 `exe` / `searched` 等字段为空。
 */
export type ProxyEngineMode = "python" | "rust";

const STORAGE_KEY = "wb.rp.engine";
/** Rust 引擎默认端口：刻意避开 Python 的 8787，便于双跑对比。 */
export const RUST_ENGINE_PORT = 8788;

export function getEngine(): ProxyEngineMode {
  try {
    return localStorage.getItem(STORAGE_KEY) === "rust" ? "rust" : "python";
  } catch {
    return "python";
  }
}

export function engineLabel(mode: ProxyEngineMode): string {
  return mode === "rust" ? "Rust 引擎（内置）" : "Python 服务（独立）";
}

/** 该引擎在当前宿主下是否可管理（webui 两个都不可管理）。 */
export function engineSupported(mode: ProxyEngineMode): boolean {
  if (!isDesktop()) return false;
  return mode === "rust" ? true : serviceSupported();
}

export function engineBase(mode: ProxyEngineMode): string {
  if (mode === "rust") return `http://127.0.0.1:${RUST_ENGINE_PORT}`;
  return DEFAULT_BASE;
}

/** 切换引擎：写入选择并把管理面 base URL 指过去。 */
export function setEngine(mode: ProxyEngineMode): string {
  try {
    localStorage.setItem(STORAGE_KEY, mode);
  } catch {
    /* 隐私模式下写不了，按本次会话生效 */
  }
  const base = isDesktop() ? engineBase(mode) : getBaseUrl();
  setBaseUrl(base);
  return base;
}

function rustStatusToService(status: {
  running?: boolean;
  baseUrl?: string;
  port?: number;
}): RpServiceStatus {
  return {
    running: Boolean(status.running),
    managed: Boolean(status.running),
    starting: false,
    pid: null,
    port: status.port ?? RUST_ENGINE_PORT,
    baseUrl: status.baseUrl ?? engineBase("rust"),
    exe: null,
    exeExists: true,
    dataDir: null,
    searched: [],
    firstRunPassword: null,
    logTail: [],
  };
}

export async function engineStatus(mode: ProxyEngineMode = getEngine()): Promise<RpServiceStatus> {
  if (mode === "rust") {
    const status = await invoke<{ running?: boolean; baseUrl?: string; port?: number }>(
      "proxy_engine_status",
    );
    return rustStatusToService(status);
  }
  return serviceStatus();
}

export async function engineStart(
  mode: ProxyEngineMode = getEngine(),
  port?: number,
): Promise<RpServiceStatus> {
  if (mode === "rust") {
    // 端口冲突时给明确错误，而不是悄悄换端口——双跑对比必须端口固定。
    const status = await invoke<{ running?: boolean; baseUrl?: string; port?: number }>(
      "proxy_engine_start",
      { port: port ?? RUST_ENGINE_PORT },
    );
    return rustStatusToService(status);
  }
  return serviceStart(port);
}

export async function engineStop(mode: ProxyEngineMode = getEngine()): Promise<RpServiceStatus> {
  if (mode === "rust") {
    const status = await invoke<{ running?: boolean; baseUrl?: string; port?: number }>(
      "proxy_engine_stop",
    );
    return rustStatusToService(status);
  }
  return serviceStop();
}
