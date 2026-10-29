/**
 * 子页共用的骨架。
 *
 * 抽在这里而不是每个页面各写一遍：14 个子页里绝大多数是「拿概览 → 渲染面板」，
 * 空态文案只该有一处，否则改文案要动 10 个文件。
 */

import { Loader2 } from "lucide-react";

import { useReverseProxy } from "./ReverseProxyLayout";

/** 概览还没到手时的占位。与面板的圆角/边框保持一致，切换时不会跳一下。 */
export function PanelSkeleton({ label = "正在读取服务状态…" }: { label?: string }) {
  return (
    <div className="flex items-center justify-center gap-2 rounded-xl border border-dashed border-border/70 px-6 py-16 text-sm text-muted-foreground">
      <Loader2 className="size-4 animate-spin" />
      {label}
    </div>
  );
}

/**
 * 「服务没跑」时的落地页。
 *
 * 刻意不做成错误提示：内置服务是随主程序启动的，用户看到这一屏通常是刚点过
 * 「停止」，或者端口全被占导致启动失败——两种情况都在布局的状态条里给了动作。
 */
export function ServiceDown() {
  return (
    <div className="rounded-xl border border-dashed border-border/70 px-6 py-16 text-center">
      <p className="text-sm font-medium">内置服务未运行</p>
      <p className="mt-1 text-xs text-muted-foreground">用页面顶部的「启动内置服务」拉起它，或到「服务设置」里换一个端口。</p>
    </div>
  );
}

/** 常见组合：服务在跑 + 概览已到手才渲染面板，否则给对应的空态。 */
export function usePanelGate() {
  const { overview, service } = useReverseProxy();
  const ready = Boolean(service?.running && overview);
  return { overview, ready };
}
