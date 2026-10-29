import { invoke } from "@tauri-apps/api/core";

import { DEFAULT_BASE, getBaseUrl, setBaseUrl } from "@/lib/reverse-proxy";

/**
 * 反代服务的进程管控（仅桌面端可用）。
 *
 * 反代服务是独立可执行文件，桌面端负责把它拉起来/停掉——用户不该为了进管理页
 * 先去开一个终端敲 `python serve.py`。只服务「反代服务old」，内置服务见 `proxy-service.ts`；
 * `serviceSupported()` 返回 false，界面据此隐藏这些控件。
 */

export type RpServiceStatus = {
  /** 端口是否在监听——「能否连上」的唯一真相。 */
  running: boolean;
  /** 由本应用拉起且进程仍存活。 */
  managed: boolean;
  /** 已拉起但端口还没起来。 */
  starting: boolean;
  pid: number | null;
  port: number;
  baseUrl: string;
  exe: string | null;
  exeExists: boolean;
  /** 密钥/令牌的存放目录（`state.json` 所在处）。定位不到服务程序时为 null。 */
  dataDir: string | null;
  /** 找不到可执行文件时，服务端尝试过的位置。 */
  searched: string[];
  /** 首次启动时服务端生成的管理员口令，仅供本次显示。 */
  firstRunPassword: string | null;
  logTail: string[];
};

export function serviceSupported(): boolean {
  // 只剩桌面端一个形态：Python old 启动器在桌面端总是可用。
  return true;
}

export function serviceStatus(): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_status");
}

export function serviceStart(port?: number): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_start", { port: port ?? null, host: null });
}

/**
 * 以指定监听地址重启服务。
 *
 * 监听地址是 uvicorn 的启动参数，进程起来后无法更改，所以改地址只能重启。
 * 桌面端托管时默认只绑 127.0.0.1；只有用户显式要求「对局域网开放」才绑 0.0.0.0。
 * 这个选择不写进配置：对外开端口应该是每次都要重新确认的动作。
 */
export function serviceRestartWithHost(host: "0.0.0.0" | "127.0.0.1"): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_restart_with_host", { host });
}

/**
 * 换端口重启服务。
 *
 * 端口和监听地址一样是 uvicorn 的启动参数，改不了只能重启。与换地址的差别是
 * **新端口必须先确认没被占** —— 先停再启时如果新端口起不来，用户会从「能用」
 * 掉到「完全不能用」，而这个失败本来可以在动手之前就拒绝掉。
 *
 * 监听姿态由 Rust 侧沿用当前值（当前绑 0.0.0.0 就还是 0.0.0.0），
 * 换端口不该顺手把服务暴露到局域网。
 */
export function serviceRestartWithPort(port: number): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_restart_with_port", { port });
}

export function serviceStop(): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_stop");
}

export function serviceSetExe(path: string | null): Promise<RpServiceStatus> {
  return invoke<RpServiceStatus>("reverse_proxy_set_exe", { path: path ?? "" });
}

/**
 * 以管理员身份停止服务（弹 UAC）。
 *
 * 正常停止是在进程内直接结束监听端口的进程；只有那个进程属于别的用户或更高权限、
 * `OpenProcess` 被拒时，才需要提权。返回值是一句给用户看的提示。
 */
export function serviceStopElevated(): Promise<string> {
  return invoke<string>("reverse_proxy_stop_elevated");
}

/** 在资源管理器里打开「密钥储存位置」（选中 state.json）。返回打开的目录。 */
export function serviceOpenDataDir(): Promise<string> {
  return invoke<string>("reverse_proxy_open_data_dir");
}

/** 保存的登录凭据；没保存过则为 null。 */
export type RpSavedLogin = {
  mode: "user" | "key";
  username?: string;
  password?: string;
  key?: string;
};

export function savedLogin(): Promise<RpSavedLogin | null> {
  return invoke<RpSavedLogin | null>("reverse_proxy_saved_login");
}

/** 保存登录凭据；传 null 表示清除（界面里取消「保存密码」）。 */
export function saveLogin(login: RpSavedLogin | null): Promise<void> {
  return invoke<void>("reverse_proxy_save_login", { login });
}

/**
 * 「自动关闭反代服务」开关的持久化读写。
 *
 * 与其它服务的差别：它不属于「进程管控」，而是退出行为 ——
 * 真相源在 Rust 侧的 `~/.wb-switch-plus/reverse_proxy.json`，
 * 退出钩子（`RunEvent::Exit`）在那里读一次决定要不要停服务。
 *
 * 所以设置页不自己存 localStorage：改在设置页、读在 Rust，存两处必然漂移。
 */
export function serviceCloseOnExitEnabled(): Promise<boolean> {
  return invoke<boolean>("reverse_proxy_close_on_exit");
}

/** 设置「退出时关闭反代服务」。返回回读**后的值（以落盘结果为准）。 */
export function setServiceCloseOnExit(enabled: boolean): Promise<boolean> {
  return invoke<boolean>("reverse_proxy_set_close_on_exit", { enabled });
}

/** 把任意错误转成给用户看的一句话（Tauri 命令抛的是字符串）。 */
export function serviceError(cause: unknown): string {
  if (typeof cause === "string" && cause.trim()) return cause;
  if (cause instanceof Error && cause.message) return cause.message;
  return String(cause);
}

/** 是否把「服务地址」跟随刚启动的本地服务——仅当当前地址是回环或默认值时。 */
export function shouldAdoptBaseUrl(): boolean {
  const current = getBaseUrl();
  if (!current || current === DEFAULT_BASE) return true;
  try {
    const host = new URL(current).hostname;
    return host === "127.0.0.1" || host === "localhost" || host === "::1" || host === "[::1]";
  } catch {
    return true;
  }
}

export function adoptBaseUrl(status: RpServiceStatus): void {
  if (shouldAdoptBaseUrl()) setBaseUrl(status.baseUrl);
}
