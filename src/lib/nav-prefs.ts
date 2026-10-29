import { useEffect, useState } from "react";

/**
 * 左侧导航入口的显示偏好。
 *
 * 语义（与设置页文案一致）：
 * - 关闭「关联会话」后，左侧导航不再渲染该入口；页面本身仍可经 URL 直达，
 *   会话数据、关联关系与账号库都不受影响，重新打开即恢复。
 * - 关闭「反代服务old」后同理：它只是回滚通道，多数用户不该看到它。
 * - 默认值：开启。
 *
 * 持久化在 localStorage（与 `wb-switch.tools.<id>` / `wb-switch.theme` 同风格），
 * 键名 `wb-switch.nav.sessions` / `wb-switch.nav.reverse-proxy-old`，
 * 值 `"1"` / `"0"`；缺省时为开启。
 */
const SESSIONS_NAV_STORAGE_KEY = "wb-switch.nav.sessions";
const LEGACY_PROXY_NAV_STORAGE_KEY = "wb-switch.nav.reverse-proxy-old";

/** 同页面内的变更通知（`storage` 事件只在其它标签页触发）。 */
const NAV_PREFS_CHANGED_EVENT = "wb-switch:nav-prefs-changed";

function readFlag(key: string): boolean {
  try {
    return localStorage.getItem(key) !== "0";
  } catch {
    return true;
  }
}

function writeFlag(key: string, enabled: boolean): void {
  try {
    localStorage.setItem(key, enabled ? "1" : "0");
  } catch {
    /* 隐私模式等场景下写入失败：开关本次会话内仍可见（由调用方 state 驱动） */
  }
  window.dispatchEvent(new Event(NAV_PREFS_CHANGED_EVENT));
}

/** 订阅某个导航开关；设置页改动后（同一标签页）侧边栏立即同步。 */
function useNavFlag(key: string): boolean {
  const [enabled, setEnabled] = useState(() => readFlag(key));
  useEffect(() => {
    const sync = () => setEnabled(readFlag(key));
    window.addEventListener("storage", sync);
    window.addEventListener(NAV_PREFS_CHANGED_EVENT, sync);
    return () => {
      window.removeEventListener("storage", sync);
      window.removeEventListener(NAV_PREFS_CHANGED_EVENT, sync);
    };
  }, [key]);
  return enabled;
}

export function isSessionsNavEnabled(): boolean {
  return readFlag(SESSIONS_NAV_STORAGE_KEY);
}

export function setSessionsNavEnabled(enabled: boolean): void {
  writeFlag(SESSIONS_NAV_STORAGE_KEY, enabled);
}

export function useSessionsNavEnabled(): boolean {
  return useNavFlag(SESSIONS_NAV_STORAGE_KEY);
}

export function isLegacyProxyNavEnabled(): boolean {
  return readFlag(LEGACY_PROXY_NAV_STORAGE_KEY);
}

export function setLegacyProxyNavEnabled(enabled: boolean): void {
  writeFlag(LEGACY_PROXY_NAV_STORAGE_KEY, enabled);
}

export function useLegacyProxyNavEnabled(): boolean {
  return useNavFlag(LEGACY_PROXY_NAV_STORAGE_KEY);
}
