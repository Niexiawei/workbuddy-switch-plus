/**
 * 「反代服务old」入口：Python 独立进程版的管理界面。
 *
 * 与内置服务的关系（见 docs/reverse-proxy-integration-plan.md §3.4 / §5 D4）：
 *   · 两套**共用同一份** `~/.wb-switch-plus/reverse-proxy/state.json` 与账号库，
 *     所以同一时刻只该有一个在跑，否则会互相覆盖；
 *   · 内置服务占 8787 起（冲突 +1），old 固定在 8788；
 *   · 这里在启动 Python 之前先停掉内置服务，并把「回滚通道」这件事说清楚。
 *
 * 页面本体沿用原来的 `ReverseProxyPage`（登录门 + 竖排 Tab），一行没重写——
 * 它的价值就是「和以前完全一样」，改它等于失去回滚的意义。
 */

import { useCallback, useEffect, useState } from "react";
import { History, Play, RefreshCw, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Banner, PanelHeader, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import { ServiceControl } from "@/components/reverse-proxy/service-control";
import { proxyServiceStatus, proxyServiceStop, type ProxyServiceStatus } from "@/lib/proxy-service";
import {
  serviceRestartWithPort,
  serviceStatus,
  serviceSupported,
  type RpServiceStatus,
} from "@/lib/reverse-proxy-service";
import ReverseProxyPage from "./ReverseProxyPage";

/** old 的固定端口：让出 8787 给内置服务，老用户按文档也能找到它。 */
export const LEGACY_PORT = 8788;

export default function LegacyReverseProxyPage() {
  const supported = serviceSupported();
  const [builtin, setBuiltin] = useState<ProxyServiceStatus | null>(null);
  const [legacy, setLegacy] = useState<RpServiceStatus | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    if (!supported) return;
    const [nextBuiltin, nextLegacy] = await Promise.all([
      proxyServiceStatus().catch(() => null),
      serviceStatus().catch(() => null),
    ]);
    setBuiltin(nextBuiltin);
    setLegacy(nextLegacy);
  }, [supported]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /**
   * 拉起 Python 版之前先停内置服务。
   *
   * 这一步不是「优化」而是必须的：两边写同一份 state.json，同秒写同一处是后写者胜，
   * 表现是密钥或审计记录凭空消失。宁可多一次确认，也不要静默丢数据。
   */
  async function startLegacy() {
    setBusy(true);
    try {
      if (builtin?.running) {
        await proxyServiceStop();
        toast.info("已停止内置服务，避免两边同时写同一份 state.json");
      }
      // 端口是 Python 进程的启动参数，改了只能重启进程，因此一律走 restart：
      // 无论 old 现在在不在跑，都落到 8788。
      const status = await serviceRestartWithPort(LEGACY_PORT);
      setLegacy(status);
      await refresh();
      toast.success(`反代服务old 已在 ${LEGACY_PORT} 运行`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  const conflict = Boolean(builtin?.running && legacy?.running);

  return (
    <div className="mx-auto w-full min-w-0 max-w-[1240px] px-6 py-8 sm:px-8">
      <PanelHeader
        title={
          <span className="flex items-center gap-2">
            <History className="size-5 text-muted-foreground" />
            反代服务old
            <ToneBadge tone="warn">Python</ToneBadge>
          </span>
        }
        desc="上一代的独立 Python 服务（wb-reverse-proxy.exe）。只作为回滚通道保留，后续会整体移除。"
        action={
          <Button variant="outline" size="sm" onClick={() => void refresh()} disabled={busy}>
            <RefreshCw className="size-3.5" />刷新
          </Button>
        }
      />

      <div className="mt-4 space-y-3">
        <Banner tone="muted">
          两套实现在同一台机器上<strong>共用</strong>一份状态文件与账号库
          （<code className="font-mono">~/.wb-switch-plus/reverse-proxy/state.json</code>）。
          同时运行会互相覆盖密钥与审计记录，所以启动 old 前会自动停掉内置服务。
        </Banner>

        {conflict ? (
          <Banner tone="bad">
            <span className="flex items-start gap-1.5">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0" />
              <span>检测到内置服务与 old 同时在运行，请先停掉其中一个，否则可能丢数据。</span>
            </span>
          </Banner>
        ) : null}

        <div className="flex flex-wrap items-center gap-x-6 gap-y-2 rounded-xl border border-border/70 bg-card px-4 py-3 text-xs">
          <span className="flex items-center gap-1.5">
            <span className="text-muted-foreground">old</span>
            <code className="font-mono">{legacy?.baseUrl ?? `http://127.0.0.1:${LEGACY_PORT}`}</code>
            {legacy?.running ? <ToneBadge tone="ok">运行中</ToneBadge> : <ToneBadge tone="muted">已停止</ToneBadge>}
          </span>
          <span className="flex items-center gap-1.5">
            <span className="text-muted-foreground">内置服务</span>
            <code className="font-mono">{builtin?.baseUrl ?? "—"}</code>
            {builtin?.running ? <ToneBadge tone="ok">运行中</ToneBadge> : <ToneBadge tone="muted">已停止</ToneBadge>}
          </span>
          {supported ? (
            <Button size="sm" onClick={() => void startLegacy()} disabled={busy}>
              <Play className="size-3.5" />
              {legacy?.running ? `重启 old（${LEGACY_PORT}）` : `在 ${LEGACY_PORT} 启动 old`}
            </Button>
          ) : null}
        </div>
      </div>

      <div className="mt-8 border-t border-border/60 pt-6">
        {legacy?.running || !supported ? (
          <ReverseProxyPage />
        ) : (
          <div className="space-y-4">
            <p className="text-xs leading-5 text-muted-foreground">
              old 还没运行。也可以直接用下面的控制台把它拉起来（首次启动会显示生成的管理口令）。
            </p>
            <ServiceControl onServiceUp={() => void refresh()} />
          </div>
        )}
      </div>
    </div>
  );
}
