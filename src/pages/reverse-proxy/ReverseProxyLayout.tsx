/**
 * 内置反代页面的布局：二级菜单 + 状态条 + `<Outlet />`。
 *
 * 历史形态是「一个页面装下 16 个竖排 Tab + 登录门 + 引擎切换器」。这里把它拆成
 * 真正的路由：每个子页可直达、可刷新、可被侧边栏高亮，面板本身一行没改。
 *
 * 与之配套的三件事同时退场（见 docs/reverse-proxy-integration-plan.md §3.3 / §3.2）：
 *   · 登录门 —— 桌面端已经登录过主程序，控制台不再要求第二次密码；
 *   · 引擎切换 —— 只剩内置 Rust 一个引擎；
 *   · 页内竖排导航 —— 与侧边栏重复，改由 App 的二级菜单承担。
 */

import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react";
import { Outlet, useNavigate } from "react-router-dom";
import { toast } from "sonner";
import { RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Banner, CopyButton, PanelHeader, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import { OAuthLoginDialog } from "@/components/oauth-login-dialog";
import { defaultAdminChannel, rp, setAdminChannel, type RpOverview } from "@/lib/reverse-proxy";
import {
  portLine,
  proxyServiceStart,
  proxyServiceStatus,
  type ProxyServiceStatus,
} from "@/lib/proxy-service";

/** 子页共享的数据与回调：概览、服务状态，以及「重新拉一次」。 */
export type ReverseProxyContextValue = {
  overview: RpOverview | null;
  service: ProxyServiceStatus | null;
  reloadOverview: () => Promise<void>;
  /** 打开发起 OAuth 扫码的对话框（由布局统一持有，账号池子页只负责触发）。 */
  openAddAccount: () => void;
};

const ReverseProxyContext = createContext<ReverseProxyContextValue | null>(null);

export function useReverseProxy(): ReverseProxyContextValue {
  const value = useContext(ReverseProxyContext);
  if (!value) throw new Error("useReverseProxy 必须在 ReverseProxyLayout 内使用");
  return value;
}

function fmtUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "—";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return `${h} 小时 ${m} 分`;
  return `${m} 分钟`;
}

function StatusFact({ label, value }: { label: string; value: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium tabular-nums">{value}</span>
    </span>
  );
}

export default function ReverseProxyLayout() {
  const navigate = useNavigate();
  const [service, setService] = useState<ProxyServiceStatus | null>(null);
  const [overview, setOverview] = useState<RpOverview | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  // 通道只装配一次：管理面走进程内 invoke，与服务的运行状态无关——
  // 停机时调用会拿到 Err（「内置服务未运行」），由下面的 phase 分支渲染提示。
  useEffect(() => {
    setAdminChannel(defaultAdminChannel());
    return () => setAdminChannel(null);
  }, []);

  const refreshService = useCallback(async () => {
    const status = await proxyServiceStatus();
    setService(status);
    return status;
  }, []);

  const reloadOverview = useCallback(async () => {
    setOverview(await rp.overview());
  }, []);

  const load = useCallback(async () => {
    setError("");
    try {
      await refreshService();
      // **不按 running 门控**：管理面走进程内通道，与数据面是否在监听无关——
      // 停机 / Failed 态下概览照样能拉（Rust 侧通道恒可用），控制台完整可用。
      await reloadOverview();
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
    }
  }, [refreshService, reloadOverview]);

  useEffect(() => {
    void load();
  }, [load]);

  // 概览 30 秒自刷新（页面可见时）。与列表页的「切 tab 才拉」不同，这里要跟上运行态。
  // 停机时也照刷：控制台在停机态仍应展示配置与池信息（通道不依赖监听）。
  useEffect(() => {
    const id = window.setInterval(() => {
      if (document.hidden) return;
      void reloadOverview().catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(id);
  }, [reloadOverview]);

  const start = useCallback(async () => {
    setBusy(true);
    setError("");
    try {
      const status = await proxyServiceStart();
      setService(status);
      await load();
      toast.success("内置服务已启动");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("启动失败");
    } finally {
      setBusy(false);
    }
  }, [load]);

  const [addOpen, setAddOpen] = useState(false);

  const context = useMemo<ReverseProxyContextValue>(() => ({
    overview,
    service,
    reloadOverview,
    openAddAccount: () => setAddOpen(true),
  }), [overview, service, reloadOverview]);

  const active = overview?.accounts.find((account) => account.active && account.enabled);
  const enabled = overview?.accounts.filter((account) => account.enabled).length ?? 0;
  const notice = portLine(service);

  return (
    <ReverseProxyContext.Provider value={context}>
      <div className="mx-auto w-full min-w-0 max-w-[1240px] px-6 py-8 sm:px-8">
        <header className="flex flex-wrap items-start justify-between gap-4">
          <PanelHeader
            title="反代服务"
            desc="把 WorkBuddy / CodeBuddy 登录态转成 OpenAI / Anthropic 兼容 API。服务随主程序启动，无需手动运行。"
          />
          <div className="flex flex-wrap items-center gap-2">
            {service?.running ? (
              <ToneBadge tone="ok">运行中</ToneBadge>
            ) : service?.phase === "failed" ? (
              <ToneBadge tone="bad">启动失败</ToneBadge>
            ) : (
              <ToneBadge tone="muted">已停止</ToneBadge>
            )}
            <Button variant="outline" size="sm" onClick={() => void load()} disabled={busy}>
              <RefreshCw className="size-3.5" />刷新
            </Button>
          </div>
        </header>

        {error ? (
          <div className="mt-4">
            <Banner tone="bad">{error}</Banner>
          </div>
        ) : null}

        {!service?.running ? (
          <div className="mt-4 flex flex-wrap items-center gap-3">
            <Banner tone={service?.phase === "failed" ? "bad" : "warn"}>{notice}</Banner>
            <Button size="sm" onClick={() => void start()} disabled={busy}>
              {busy ? "正在启动…" : "启动内置服务"}
            </Button>
            <Button variant="ghost" size="sm" onClick={() => navigate("/reverse-proxy/service")}>
              改端口
            </Button>
          </div>
        ) : null}

        {/* 常驻状态条：切到哪个子页都能看到地址与关键计数。 */}
        <div className="mt-5 flex flex-wrap items-center gap-x-5 gap-y-2 rounded-xl border border-border/70 bg-card px-4 py-3 text-xs">
          <span className="flex items-center gap-2">
            <span className="text-muted-foreground">服务地址</span>
            <code className="font-mono">{service?.baseUrl ?? "—"}</code>
            {service?.baseUrl ? <CopyButton value={service.baseUrl} label="" className="h-6 px-2" /> : null}
          </span>
          {service?.portShifted ? <ToneBadge tone="warn">{notice}</ToneBadge> : null}
          <StatusFact label="已运行" value={overview ? fmtUptime(overview.uptime) : "—"} />
          <StatusFact label="账号" value={overview ? `${enabled} / ${overview.accounts.length} 启用` : "—"} />
          <StatusFact label="API 密钥" value={overview ? String(overview.keys.length) : "—"} />
          <StatusFact label="当前账号" value={active ? active.name : "无"} />
        </div>

        <div className="mt-6 min-w-0">
          <Outlet />
        </div>
      </div>

      {/* OAuth 扫码由布局持有：账号池子页只触发，不各自维护一份 open 状态。 */}
      <OAuthLoginDialog
        open={addOpen}
        onOpenChange={(open) => {
          setAddOpen(open);
          if (!open) void reloadOverview().catch(() => undefined);
        }}
      />
    </ReverseProxyContext.Provider>
  );
}
