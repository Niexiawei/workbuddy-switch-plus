import { useEffect, useState } from "react";
import { toast } from "sonner";
import { BrowserRouter, HashRouter, Navigate, Outlet, Route, Routes, useLocation } from "react-router-dom";
import { ArrowUp, ChevronRight, History, Link2, Loader2, MessagesSquare, Play, Server, Settings, Sparkles, User } from "lucide-react";

import { cn } from "@/lib/utils";
import * as api from "@/lib/api";
import AccountsPage from "@/pages/AccountsPage";
import SessionsPage from "@/pages/SessionsPage";
import CreditStatsPage from "@/pages/CreditStatsPage";
import TokenStatsPage from "@/pages/TokenStatsPage";
import SettingsPage from "@/pages/SettingsPage";
import ReverseProxyLayout from "@/pages/reverse-proxy/ReverseProxyLayout";
import OverviewPage from "@/pages/reverse-proxy/OverviewPage";
import LogsPage from "@/pages/reverse-proxy/LogsPage";
import RpAccountsPage from "@/pages/reverse-proxy/AccountsPage";
import GroupsPage from "@/pages/reverse-proxy/GroupsPage";
import EgressPage from "@/pages/reverse-proxy/EgressPage";
import KeysPage from "@/pages/reverse-proxy/KeysPage";
import CliPage from "@/pages/reverse-proxy/CliPage";
import GuidePage from "@/pages/reverse-proxy/GuidePage";
import ModelsPage from "@/pages/reverse-proxy/ModelsPage";
import ServicePage from "@/pages/reverse-proxy/ServicePage";
import AlertsPage from "@/pages/reverse-proxy/AlertsPage";
import TokensPage from "@/pages/reverse-proxy/TokensPage";
import UsersPage from "@/pages/reverse-proxy/UsersPage";
import SecurityPage from "@/pages/reverse-proxy/SecurityPage";
import AuditPage from "@/pages/reverse-proxy/AuditPage";
import LegacyReverseProxyPage from "@/pages/reverse-proxy/legacy/LegacyReverseProxyPage";
import {
  REVERSE_PROXY_DEFAULT_SEGMENT,
  REVERSE_PROXY_NAV,
} from "@/pages/reverse-proxy/nav";
import { SidebarGroupLabel, SidebarNavItem, SidebarSubItem } from "@/components/sidebar-nav";
import { StatusDot, AppIconMark } from "@/components/product-marks";
import { CompanionDemoDialog } from "@/components/companion-demo-dialog";
import { UpdateInstallDialog } from "@/components/update-install-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator, DropdownMenuTrigger } from "@/components/ui/dropdown-menu";
import { Toaster } from "@/components/ui/sonner";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import companionTrayIcon from "@/assets/agent-companion-tray.png";
import { DEMO_UNAVAILABLE_MESSAGE, demoModeEnabled, pagesDemoHostingEnabled } from "@/lib/demo-mode";
import { useLegacyProxyNavEnabled, useSessionsNavEnabled } from "@/lib/nav-prefs";
import { changeCompanionEnabled, useCompanionEnabled } from "@/lib/use-companion-enabled";
import { useCreditAutoRefresh } from "@/lib/use-credit-auto-refresh";
import { useRotateDeferredNotice } from "@/lib/use-rotate-deferred-notice";
import { useUpdateState } from "@/lib/use-update-state";
import { useWorkbuddyStatusRefresh } from "@/lib/use-workbuddy-status-refresh";
import { useAccountsStore } from "@/stores/accounts";

/**
 * 悬浮窗面板：从 footer 向上滑出，开关与设置同屏。
 *
 * footer 只保留一个「图标位」——点图标弹面板，而不是并排第二个按钮：
 * 开关与设置都在面板里，关掉悬浮窗时也不必留一个灰掉的死图标。
 * 面板关闭后进入设置的常驻路径仍是：设置页 → Agent Companion → 悬浮窗设置。
 */
const COMPANION_MENU_CONTENT = { side: "top", align: "start", sideOffset: 8, className: "w-56" } as const;

function CompanionFooter() {
  const { enabled, busy } = useCompanionEnabled();
  /** 面板打开时收起 hover tooltip——两者都朝上弹，同时出现会叠在一起。 */
  const [menuOpen, setMenuOpen] = useState(false);
  const [tipOpen, setTipOpen] = useState(false);

  async function onToggle() {
    if (enabled === null) return;
    try {
      const confirmed = await changeCompanionEnabled(!enabled);
      toast.success(confirmed ? "已启用 Agent Companion 悬浮窗" : "已关闭 Agent Companion 悬浮窗");
    } catch (cause) {
      toast.error("悬浮窗设置失败", { description: api.asError(cause) });
    }
  }

  async function openSettings() {
    try {
      await api.openCompanionSettings();
    } catch (cause) {
      toast.error("打开悬浮窗设置失败", { description: api.asError(cause) });
    }
  }

  return (
    <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
      <Tooltip open={tipOpen && !menuOpen} onOpenChange={setTipOpen}>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="icon" className="size-8 shrink-0 rounded-lg" aria-label="会话悬浮窗" disabled={enabled === null || busy}>
              <img src={companionTrayIcon} alt="" className={cn("size-6 object-contain transition-all", !enabled && "grayscale opacity-55")} />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">会话悬浮窗</TooltipContent>
      </Tooltip>
      <DropdownMenuContent {...COMPANION_MENU_CONTENT}>
        {/* 整行即开关（方向键 / Enter 同语义）；行尾 Switch 只作视觉，点击穿透到行上，避免出现两个焦点。 */}
        <DropdownMenuItem
          className="gap-3"
          disabled={enabled === null || busy}
          onSelect={(event) => {
            // 面板保持打开：切换是异步的，让用户看到开关落定。
            event.preventDefault();
            void onToggle();
          }}
        >
          <span className="min-w-0 flex-1">会话悬浮窗</span>
          <Switch checked={Boolean(enabled)} tabIndex={-1} aria-hidden className="pointer-events-none" />
        </DropdownMenuItem>
        <p className="px-2.5 pb-1 text-xs leading-4 text-muted-foreground">
          {enabled ? "关闭后悬浮栏将隐藏，并停止 wb-switch 中的会话监听。" : "开启后会显示悬浮栏，并开始监听会话状态。"}
        </p>
        <DropdownMenuSeparator />
        <DropdownMenuItem disabled={enabled === null || busy || !enabled} onSelect={() => void openSettings()}>
          <Settings />悬浮窗设置
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/**
 * 演示模式的悬浮窗入口：与桌面正式版同形（一个图标位 + 向上弹出的面板），
 * 但面板里的开关与设置只提示「演示不可用」，不触发任何本机命令；
 * 多一条「查看悬浮栏演示」，打开只读演示浮层。
 */
function CompanionDemoFooter() {
  const [open, setOpen] = useState(false);
  /** 面板打开时收起 hover tooltip，与正式版同一处理。 */
  const [menuOpen, setMenuOpen] = useState(false);
  const [tipOpen, setTipOpen] = useState(false);

  /** 演示面板里的行：保持可聚焦（键盘也能走一遍），但只说明不可用。 */
  function unavailable(event: Event) {
    event.preventDefault();
    toast.info(DEMO_UNAVAILABLE_MESSAGE);
  }

  return (
    <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
      <Tooltip open={tipOpen && !menuOpen} onOpenChange={setTipOpen}>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button type="button" variant="ghost" size="icon" className="size-8 shrink-0 rounded-lg" aria-label="会话悬浮窗">
              <img src={companionTrayIcon} alt="" className="size-6 object-contain" />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">会话悬浮窗</TooltipContent>
      </Tooltip>
      <DropdownMenuContent {...COMPANION_MENU_CONTENT}>
        <DropdownMenuItem className="gap-3" onSelect={unavailable}>
          <span className="min-w-0 flex-1">会话悬浮窗</span>
          <Switch checked tabIndex={-1} aria-hidden className="pointer-events-none" />
        </DropdownMenuItem>
        <p className="px-2.5 pb-1 text-xs leading-4 text-muted-foreground">关闭后悬浮栏将隐藏，并停止 wb-switch 中的会话监听。</p>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={unavailable}>
          <Settings />悬浮窗设置
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={() => setOpen(true)}>
          <Play />查看悬浮栏演示
        </DropdownMenuItem>
      </DropdownMenuContent>
      <CompanionDemoDialog open={open} onOpenChange={setOpen} />
    </DropdownMenu>
  );
}

function UpdateCenter({ running }: { running: boolean | undefined }) {
  const version = useAccountsStore((s) => s.status?.version);
  const snapshot = useUpdateState();
  const [dialogOpen, setDialogOpen] = useState(false);
  const showCompanion = api.isDesktop() && !demoModeEnabled;
  // 演示模式只提供只读演示浮层，不渲染正式版的悬浮窗开关。
  const showCompanionDemo = demoModeEnabled;

  // 阶段由 Rust 更新服务经 `update-state` 推送（托盘同源），前端不再轮询检查。
  // 已知目标版本时，检查中 / 失败也要保留入口，与托盘「升级到 vX / 点击重试」对齐。
  const hasKnownTarget = Boolean(snapshot.latest);
  const hasUpdate =
    snapshot.phase === "available" ||
    snapshot.phase === "downloading" ||
    snapshot.phase === "readyToRestart" ||
    (hasKnownTarget && (snapshot.phase === "error" || snapshot.phase === "checking"));
  const updateHint =
    snapshot.phase === "downloading"
      ? snapshot.percent === null
        ? "正在下载更新…"
        : `正在下载更新 ${snapshot.percent}%`
      : snapshot.phase === "readyToRestart"
        ? "重启以完成升级"
        : snapshot.phase === "error"
          ? "更新失败，点击重试"
          : snapshot.phase === "checking"
            ? "正在检查…"
            : "更新";

  return (
    <>
      <section className="mt-auto border-t border-sidebar-border px-2 pt-3 text-xs">
        <div className="flex items-center gap-2 text-[13px] text-sidebar-foreground">
          {showCompanion ? <CompanionFooter /> : showCompanionDemo ? <CompanionDemoFooter /> : (
            <StatusDot on={Boolean(running)} />
          )}
          {/* 名字必须常显：以前它只在「没有悬浮窗入口」的分支里渲染，
              桌面端（有 Companion 时）左下角就只剩一个图标和版本号。 */}
          <span className="min-w-0 flex-1 truncate">WB-Switch-Plus</span>
          <div className="ml-auto flex shrink-0 items-center gap-1.5">
            <span className="text-sidebar-foreground/50">v{version || "?"}</span>
            {hasUpdate && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    size="icon"
                    className="size-5 rounded-full p-0"
                    aria-label={updateHint}
                    onClick={() => setDialogOpen(true)}
                  >
                    {snapshot.phase === "downloading" || snapshot.phase === "checking" ? (
                      <Loader2 className="size-3 animate-spin" strokeWidth={2.5} aria-hidden="true" />
                    ) : (
                      <ArrowUp className="size-3" strokeWidth={2.5} aria-hidden="true" />
                    )}
                  </Button>
                </TooltipTrigger>
                <TooltipContent side="top">{updateHint}</TooltipContent>
              </Tooltip>
            )}
          </div>
        </div>
      </section>
      <UpdateInstallDialog open={dialogOpen} onOpenChange={setDialogOpen} />
    </>
  );
}

function Layout() {
  const running = useAccountsStore((s) => s.status?.running);
  const hasUnifiedTitleBar =
    api.isDesktop() && typeof navigator !== "undefined" && navigator.userAgent.includes("Macintosh");
  const sessionsNavEnabled = useSessionsNavEnabled();
  const legacyProxyNavEnabled = useLegacyProxyNavEnabled();
  useCreditAutoRefresh();
  useWorkbuddyStatusRefresh();
  useRotateDeferredNotice();

  return (
    <div className="flex h-screen min-h-0 overflow-hidden bg-background">
      {hasUnifiedTitleBar ? (
        <div
          data-tauri-drag-region
          className="fixed inset-x-0 top-0 z-50 h-8"
          aria-hidden="true"
        />
      ) : null}
      <aside
        className={cn(
          "flex min-h-0 w-[220px] shrink-0 flex-col border-r border-sidebar-border bg-sidebar px-3 pb-4",
          hasUnifiedTitleBar ? "pt-20" : "pt-4",
        )}
      >
        <div className="flex items-center gap-2.5 px-1 pb-5">
          <AppIconMark size={36} className="drop-shadow-sm" />
          <div className="min-w-0">
            <div
              className="truncate text-[15px] leading-5 tracking-[-0.02em] text-sidebar-foreground/90"
              style={{
                fontFamily: '"Bricolage Grotesque Variable", "SF Pro Display", ui-sans-serif, sans-serif',
                fontWeight: 640,
              }}
            >
              WB-Switch-Plus
            </div>
            {demoModeEnabled && (
              <Badge variant="secondary" className="mt-1 h-5 border-0 px-1.5 text-[10px] text-sidebar-foreground/60 shadow-none">
                演示模式
              </Badge>
            )}
          </div>
        </div>
        <nav className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto" aria-label="主导航">
          <SidebarNavItem to="/" icon={User} label="账号管理" end />
          {sessionsNavEnabled && <SidebarNavItem to="/sessions" icon={Link2} label="关联会话" />}
          <SidebarNavItem to="/token-stats" icon={MessagesSquare} label="Token 统计" />
          <SidebarNavItem to="/credit-stats" icon={Sparkles} label="积分统计" />
          <ReverseProxyNav />
          {legacyProxyNavEnabled && <SidebarNavItem to="/reverse-proxy-old" icon={History} label="反代服务old" />}
          <SidebarNavItem to="/settings" icon={Settings} label="设置" />
        </nav>
        {api.isWebui() && !demoModeEnabled ? null : <UpdateCenter running={running} />}
      </aside>
      <main
        className={cn(
          "min-w-0 flex-1 overflow-y-auto bg-background overscroll-contain",
          hasUnifiedTitleBar && "pt-16 [&>div]:pt-4",
        )}
      >
        <Outlet />
      </main>
    </div>
  );
}

/**
 * 「反代服务」的二级菜单。
 *
 * 展开状态跟着当前路由走：只要 URL 在 `/reverse-proxy/*` 下就展开，离开也不强制
 * 收起（用户可能是先点开菜单再看别处）。额外的开合状态只存在组件内，
 * 不落 localStorage——它是一个纯粹的当下动作，不值得跨会话记住。
 */
function ReverseProxyNav() {
  const location = useLocation();
  const inside = location.pathname.startsWith("/reverse-proxy");
  const [open, setOpen] = useState(inside);
  // 从别处跳进反代页（比如概览里的「查看接入指南」）时要自动展开，
  // 否则菜单停在收起状态，用户看不出自己在哪一级。
  useEffect(() => {
    if (inside) setOpen(true);
  }, [inside]);

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        className={cn(
          "flex w-full items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
          inside
            ? "bg-foreground/[0.06] font-medium text-foreground"
            : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
        )}
      >
        <Server className="size-4 shrink-0" />
        <span className="min-w-0 flex-1 truncate text-left">反代服务</span>
        <ChevronRight className={cn("size-3.5 shrink-0 transition-transform", open && "rotate-90")} />
      </button>
      {open ? (
        <div className="mt-0.5 flex flex-col gap-0.5">
          {REVERSE_PROXY_NAV.map((group, index) =>
            group.title ? (
              <div key={`group-${group.title}`}>
                <SidebarGroupLabel>{group.title}</SidebarGroupLabel>
                {group.items.map((item) => (
                  <SidebarSubItem
                    key={item.segment}
                    to={`/reverse-proxy/${item.segment}`}
                    label={item.label}
                  />
                ))}
              </div>
            ) : (
              <div key={`group-${index}`} className="flex flex-col gap-0.5">
                {group.items.map((item) => (
                  <SidebarSubItem
                    key={item.segment}
                    to={`/reverse-proxy/${item.segment}`}
                    label={item.label}
                  />
                ))}
              </div>
            ),
          )}
        </div>
      ) : null}
    </div>
  );
}

export default function App() {
  const Router = pagesDemoHostingEnabled ? HashRouter : BrowserRouter;

  return (
    <TooltipProvider delayDuration={250}>
      <Router>
        <Routes>
          <Route element={<Layout />}>
            <Route path="/" element={<AccountsPage />} />
            <Route path="/sessions" element={<SessionsPage />} />
            <Route path="/credit-stats" element={<CreditStatsPage />} />
            <Route path="/token-stats" element={<TokenStatsPage />} />
            {/*
              反代服务：布局 + 子路由。默认段给个重定向，免得 `/reverse-proxy`
              落在一个空布局上。Python 版另走 `/reverse-proxy-old`。
            */}
            <Route path="/reverse-proxy" element={<ReverseProxyLayout />}>
              <Route index element={<Navigate to={REVERSE_PROXY_DEFAULT_SEGMENT} replace />} />
              <Route path="overview" element={<OverviewPage />} />
              <Route path="logs" element={<LogsPage />} />
              <Route path="accounts" element={<RpAccountsPage />} />
              <Route path="groups" element={<GroupsPage />} />
              <Route path="egress" element={<EgressPage />} />
              <Route path="keys" element={<KeysPage />} />
              <Route path="cli" element={<CliPage />} />
              <Route path="guide" element={<GuidePage />} />
              <Route path="models" element={<ModelsPage />} />
              <Route path="service" element={<ServicePage />} />
              <Route path="alerts" element={<AlertsPage />} />
              <Route path="tokens" element={<TokensPage />} />
              <Route path="users" element={<UsersPage />} />
              <Route path="security" element={<SecurityPage />} />
              <Route path="audit" element={<AuditPage />} />
              <Route path="*" element={<Navigate to={REVERSE_PROXY_DEFAULT_SEGMENT} replace />} />
            </Route>
            {/* Python 版（回滚通道）：独立入口，与内置服务互斥，见 LegacyReverseProxyPage。 */}
            <Route path="/reverse-proxy-old" element={<LegacyReverseProxyPage />} />
            <Route path="/settings" element={<SettingsPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
        <Toaster />
      </Router>
    </TooltipProvider>
  );
}
