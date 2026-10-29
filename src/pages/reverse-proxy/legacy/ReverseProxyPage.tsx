import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";
import {
  Activity, BookOpen, Boxes, KeyRound, LayoutDashboard, LogOut, MailWarning, Network, PlugZap, RefreshCw, ScrollText,
  Server, Settings2, ShieldCheck, Terminal, Ticket, Users, Layers,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  RpError, getBaseUrl, getToken, rp, setBaseUrl, setSessionUser, setToken,
  type RpOverview, type RpSession, type RpToken, type RpUser,
} from "@/lib/reverse-proxy";
import { saveLogin, savedLogin, serviceSupported, type RpSavedLogin } from "@/lib/reverse-proxy-service";
// 引擎切换已退场（决策 4）：这一页只剩 Python「反代服务old」一条链路。
import { OAuthLoginDialog } from "@/components/oauth-login-dialog";
import { performSignOut, shouldAutoLogin } from "@/lib/sign-out";
import { AddAccountDialog } from "@/components/reverse-proxy/add-account-dialog";
import { BatchScanDialog } from "@/components/reverse-proxy/batch-scan-dialog";
import { AccountsPanel } from "@/components/reverse-proxy/accounts-panel";
import { CliLinkCard } from "@/components/reverse-proxy/cli-link-card";
import { AlertsPanel } from "@/components/reverse-proxy/alerts-panel";
import { AuditPanel } from "@/components/reverse-proxy/audit-panel";
import { EgressPanel } from "@/components/reverse-proxy/egress-panel";
import { GroupsPanel } from "@/components/reverse-proxy/groups-panel";
import { GuidePanel, TestPanel } from "@/components/reverse-proxy/test-guide-panel";
import { KeysPanel } from "@/components/reverse-proxy/keys-panel";
import { ModelsUsagePanel } from "@/components/reverse-proxy/models-usage-panel";
import { OverviewPanel } from "@/components/reverse-proxy/overview-panel";
import { RequestLogPanel } from "@/components/reverse-proxy/request-log-panel";
import { SecurityPanel } from "@/components/reverse-proxy/security-panel";
import { ServiceControl } from "@/components/reverse-proxy/service-control";
import { TokensPanel } from "@/components/reverse-proxy/tokens-panel";
import { UsersPanel } from "@/components/reverse-proxy/users-panel";
import { ServiceSettingsPanel } from "@/components/reverse-proxy/service-settings-panel";
import { Banner, CopyButton, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import { cn } from "@/lib/utils";

type Phase = "checking" | "gate" | "ready";



/** 导航分组：按「看一眼 / 管什么 / 怎么接 / 谁在用」归拢，避免八个平级 Tab 铺成一排。 */
const NAV: { group: string; items: { value: string; label: string; icon: typeof Server }[] }[] = [
  {
    group: "运行",
    items: [
      { value: "overview", label: "概览", icon: LayoutDashboard },
      { value: "logs", label: "请求日志", icon: ScrollText },
    ],
  },
  {
    group: "账号",
    items: [
      { value: "accounts", label: "账号池", icon: Users },
      { value: "groups", label: "分组", icon: Layers },
      { value: "egress", label: "出口线路", icon: Network },
    ],
  },
  {
    group: "接入",
    items: [
      { value: "keys", label: "API 密钥", icon: KeyRound },
      // CLI 接入是「密钥」的下游动作：先有密钥，再把它写进 CLI 配置。
      { value: "cli", label: "CodeBuddy CLI", icon: Terminal },
      { value: "guide", label: "接入指南", icon: BookOpen },
    ],
  },
  {
    group: "模型",
    items: [
      // 用量统计与模型别名早就在后端跑（每个请求都落桶、别名在真实链路上解析），
      // 但一直没有界面 —— 这两个页签是它们第一次可见。
      { value: "models", label: "模型与用量", icon: Boxes },
    ],
  },
  {
    group: "服务",
    items: [
      { value: "service-settings", label: "服务设置", icon: Settings2 },
      // 告警放「服务」组而不是「运维」：它和站点名称一样是配置项，
      // 不是日常盯的运维面板。放在这里也和网页端「设置」页对得上。
      { value: "alerts", label: "告警通知", icon: MailWarning },
    ],
  },
  {
    group: "运维",
    items: [
      { value: "tokens", label: "管理令牌", icon: Ticket },
      // 既有项。标签从「用户」改成「后台账号」：它管的是 username + 口令的
      // 控制台登录账号（用来登录 8787 后台），不是上面那套终端用户。
      { value: "users", label: "后台账号", icon: ShieldCheck },
      { value: "security", label: "IP 安全", icon: Activity },
      // 审计日志后端一直有（`/admin/api/audit-logs`），但桌面端与网页端都没调过。
      // 出事之后第一个要问的就是「这条是什么时候被谁改的」，没有入口等于没有这个能力。
      { value: "audit", label: "审计日志", icon: ScrollText },
      { value: "test", label: "连接测试", icon: PlugZap },
    ],
  },
];

function fmtUptime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return "—";
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return `${h} 小时 ${m} 分`;
  return `${m} 分钟`;
}

export default function ReverseProxyPage() {
  const [phase, setPhase] = useState<Phase>("checking");
  const [session, setSession] = useState<RpSession | null>(null);
  const [overview, setOverview] = useState<RpOverview | null>(null);
  const [tokens, setTokens] = useState<RpToken[]>([]);
  const [users, setUsers] = useState<RpUser[]>([]);
  const [tab, setTab] = useState("overview");
  const [addOpen, setAddOpen] = useState(false);
  const [batchOpen, setBatchOpen] = useState(false);
  const visibleRef = useRef(true);

  const loadOverview = useCallback(async () => {
    setOverview(await rp.overview());
  }, []);

  const loadAccess = useCallback(async () => {
    const [t, u] = await Promise.all([rp.listTokens().catch(() => []), rp.listUsers().catch(() => [])]);
    setTokens(t);
    setUsers(u);
  }, []);

  const enter = useCallback(async () => {
    const s = await rp.session();
    setSession({ username: s.username, role: s.role, via: s.via });
    setSessionUser({ username: s.username, role: s.role });
    await loadOverview();
    void loadAccess();
    setPhase("ready");
  }, [loadOverview, loadAccess]);

  const bootstrap = useCallback(async () => {
    if (!getToken()) { setPhase("gate"); return; }
    setPhase("checking");
    try {
      await enter();
    } catch (cause) {
      if (cause instanceof RpError && cause.status === 401) setToken(null);
      else if (cause instanceof RpError && cause.status === 0) toast.error(cause.message);
      setPhase("gate");
    }
  }, [enter]);

  useEffect(() => { void bootstrap(); }, [bootstrap]);

  // 每 30 秒刷新概览（页面可见且不在登录页时）。
  useEffect(() => {
    if (phase !== "ready") return;
    const id = window.setInterval(() => {
      if (document.hidden || !visibleRef.current) return;
      void loadOverview().catch(() => undefined);
    }, 30000);
    return () => window.clearInterval(id);
  }, [phase, loadOverview]);

  /**
   * 退出登录。幂等是硬要求：无论服务端可达与否、是否已登出，点退出都必须回到登录页，
   * 而且必须停在登录页。
   *
   * ── 为什么点退出「还是退不出去」──
   *
   * 光清内存里的 token 是不够的。桌面端把「保存密码」写进了
   * `~/.wb-switch-plus/reverse-proxy-login.json`，而登录卡挂载时会读它并自动提交登录
   * （见 LoginCard 的 restore 副作用）。于是：
   *
   *   点退出 → 清 token → phase="gate" → 渲染登录卡 → 挂载副作用读回保存的口令
   *          → 立刻自动登录成功 → 又回到控制台
   *
   * 用户看到的就是「点了退出没反应」——看起来像退出失败，其实是退出成功后又马上登回来了。
   * 那份文件里存着明文口令。
   *
   * 所以退出必须连同保存的凭据一起清掉，并立一个「本次会话已显式退出」的标记，
   * 让登录卡不再自动登录。标记只活在内存里（组件重挂载也不复位），
   * 这样「打开软件 → 自动登录」的原有便利不受影响，只有显式点退出这一次不自动登录。
   *
   * 顺序仍然是先清本地、再通知服务端：桌面端的目标是「立刻回到登录页」，
   * 不能因为一次网络请求失败就卡住。反过来写（先 await 服务端、失败就 return）
   * 同样会表现为「点了退出没反应」。
   */
  function signOut() {
    // 具体顺序与容错都收在 `lib/sign-out.ts` 里（那是有单测的地方）：
    // 同步清本地 → 同步立「禁止自动登录」标记 → 异步清保存的凭据 / 通知服务端。
    void performSignOut({
      clearLocal: () => {
        setToken(null);
        setSessionUser(null);
        setSession(null);
        setOverview(null);
        setPhase("gate");
      },
      blockAutoLogin: () => { /* 标记由 performSignOut 自己置位 */ },
      clearSavedLogin: () => saveLogin(null),
      notifyServer: () => rp.logout(),
    });
  }

  /**
   * 这一页只服务 Python 反代服务old，它的「添加账号」有两条链路：
   *
   *   · 桌面端：OAuth 扫码走主项目（core 的 oauth 模块，直接写 accounts.json），
   *     与账户管理页是同一个流程 —— 加完两个页面同时看得到，不需要反代再实现一遍。
   *   · 备用：反代自己的 `/admin/api/oauth/*`（Python 侧实现，保留兜底）。
   */
  // 只剩桌面端一个形态：OAuth 扫码走主项目，不再分支。
  const desktop = true;

  const counts = useMemo<Record<string, number | undefined>>(() => ({
    accounts: overview?.accounts.length,
    keys: overview?.keys.length,
    tokens: tokens.length,
    users: users.length,
  }), [overview, tokens, users]);

  if (phase !== "ready" || !overview || !session) {
    return <ConnectView checking={phase === "checking"} onSuccess={() => void bootstrap()} />;
  }

  const active = overview.accounts.find((a) => a.active && a.enabled);
  const enabled = overview.accounts.filter((a) => a.enabled).length;

  return (
    <div className="mx-auto w-full min-w-0 max-w-[1240px] px-6 py-8 sm:px-8">
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-xl font-semibold tracking-tight">
            <Server className="size-5 text-primary" />反代服务
          </h1>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted-foreground">
            把 WorkBuddy / CodeBuddy 登录态转成 OpenAI / Anthropic 兼容 API，并直读 workbuddy-switch 账号库。
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <ToneBadge tone="warn">Python 版</ToneBadge>
          <ToneBadge tone={session.role === "admin" ? "ok" : "muted"}>
            <ShieldCheck className="size-3" />{session.username} · {session.role === "admin" ? "管理员" : "只读"}
          </ToneBadge>
          <Button variant="outline" size="sm" onClick={() => void loadOverview().catch((e) => toast.error(e.message))}>
            <RefreshCw className="size-3.5" />刷新
          </Button>
          <Button variant="ghost" size="sm" onClick={signOut}>
            <LogOut className="size-3.5" />退出
          </Button>
        </div>
      </header>

      {/* 常驻状态条：服务地址与几个关键计数，切到哪个页都能看到。 */}
      <div className="mt-5 flex flex-wrap items-center gap-x-5 gap-y-2 rounded-xl border border-border/70 bg-card px-4 py-3 text-xs">
        <span className="flex items-center gap-2">
          <span className="text-muted-foreground">服务地址</span>
          <code className="font-mono">{getBaseUrl()}</code>
          <CopyButton value={getBaseUrl()} label="" className="h-6 px-2" />
        </span>
        <StatusFact label="已运行" value={fmtUptime(overview.uptime)} />
        <StatusFact label="账号" value={`${enabled} / ${overview.accounts.length} 启用`} />
        <StatusFact label="API 密钥" value={String(overview.keys.length)} />
        <StatusFact label="当前账号" value={active ? active.name : "无"} />
      </div>

      <Tabs
        value={tab}
        onValueChange={setTab}
        orientation="vertical"
        className="mt-6 grid items-start gap-6 lg:grid-cols-[208px_minmax(0,1fr)]"
      >
        <TabsList className="flex h-auto w-full flex-row gap-0.5 overflow-x-auto rounded-xl border border-border/70 bg-card p-2 lg:sticky lg:top-4 lg:flex-col lg:items-stretch lg:overflow-visible">
          {NAV.map((section) => (
            <div key={section.group} className="lg:w-full">
              <div className="hidden px-2 pt-2 pb-1 text-[10px] font-medium tracking-wider text-muted-foreground/70 uppercase lg:block">
                {section.group}
              </div>
              <div className="flex flex-row gap-0.5 lg:flex-col">
                {section.items.map((item) => (
                  <TabsTrigger
                    key={item.value}
                    value={item.value}
                    className="h-auto shrink-0 justify-start gap-2 rounded-lg px-2.5 py-2 text-xs data-[state=active]:bg-primary/10 data-[state=active]:text-foreground data-[state=active]:shadow-none lg:w-full"
                  >
                    <item.icon className="size-3.5 shrink-0" />
                    <span className="whitespace-nowrap">{item.label}</span>
                    {counts[item.value] !== undefined ? (
                      <span className="ml-auto hidden text-[10px] tabular-nums text-muted-foreground lg:inline">
                        {counts[item.value]}
                      </span>
                    ) : null}
                  </TabsTrigger>
                ))}
              </div>
            </div>
          ))}
        </TabsList>

        <div className="min-w-0">
          <TabsContent value="overview" className="mt-0">
            <OverviewPanel overview={overview} onReload={loadOverview} onGoto={setTab} />
          </TabsContent>
          <TabsContent value="logs" className="mt-0">
            {/* 分组归属要靠 overview 里的 keys[].groups 与 groups[]（id→name）；
               日志行本身只有 key_id。只传这两项，不把整个 overview 递进去。 */}
            <RequestLogPanel keys={overview.keys} groups={overview.groups} />
          </TabsContent>
          <TabsContent value="accounts" className="mt-0">
            <AccountsPanel overview={overview} reload={loadOverview} onAddAccount={() => setAddOpen(true)} onBatchScan={() => setBatchOpen(true)} />
          </TabsContent>
          <TabsContent value="groups" className="mt-0">
            <GroupsPanel />
          </TabsContent>
          <TabsContent value="egress" className="mt-0">
            <EgressPanel />
          </TabsContent>
          <TabsContent value="keys" className="mt-0">
            <KeysPanel overview={overview} reload={loadOverview} />
          </TabsContent>
          <TabsContent value="cli" className="mt-0">
            <CliLinkCard />
          </TabsContent>
          <TabsContent value="models" className="mt-0">
            <ModelsUsagePanel />
          </TabsContent>
          <TabsContent value="service-settings" className="mt-0">
            <ServiceSettingsPanel />
          </TabsContent>
          <TabsContent value="alerts" className="mt-0">
            <AlertsPanel />
          </TabsContent>
          <TabsContent value="tokens" className="mt-0">
            <TokensPanel tokens={tokens} reload={loadAccess} />
          </TabsContent>
          <TabsContent value="users" className="mt-0">
            <UsersPanel users={users} session={session} reload={loadAccess} onRelogin={bootstrap} />
          </TabsContent>
          <TabsContent value="security" className="mt-0">
            <SecurityPanel />
          </TabsContent>
          <TabsContent value="audit" className="mt-0">
            <AuditPanel />
          </TabsContent>
          <TabsContent value="test" className="mt-0">
            <TestPanel overview={overview} active={active} />
          </TabsContent>
          <TabsContent value="guide" className="mt-0">
            <GuidePanel />
          </TabsContent>
        </div>
      </Tabs>

      {desktop ? (
        <OAuthLoginDialog
          open={addOpen}
          onOpenChange={(open) => {
            setAddOpen(open);
            // 关窗时刷新一次：OAuth 成功后账号库已经变了，概览要跟上。
            if (!open) void loadOverview().catch(() => undefined);
          }}
        />
      ) : (
        <>
          <AddAccountDialog open={addOpen} onOpenChange={setAddOpen} onAdded={() => void loadOverview().catch(() => undefined)} />
          <BatchScanDialog open={batchOpen} onOpenChange={setBatchOpen} onAdded={() => void loadOverview().catch(() => undefined)} />
        </>
      )}
    </div>
  );
}

function StatusFact({ label, value }: { label: string; value: string }) {
  return (
    <span className="flex items-center gap-1.5">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium tabular-nums">{value}</span>
    </span>
  );
}

/**
 * 未连接时的落地页。
 *
 * 桌面端把「启动服务」放在最显眼的位置——绝大多数「连不上」就是服务没起来，
 * 不该让用户先去开终端。登录表单退到右侧。
 */
function ConnectView({ checking, onSuccess }: { checking: boolean; onSuccess: () => void }) {
  const supported = serviceSupported();
  return (
    <div className="mx-auto w-full max-w-[1000px] px-6 py-10 sm:px-8">
      <header className="text-center">
        <div className="mx-auto flex size-11 items-center justify-center rounded-xl bg-primary/10">
          <Server className="size-5 text-primary" />
        </div>
        <h1 className="mt-3 text-lg font-semibold tracking-tight">反代服务</h1>
        <p className="mx-auto mt-1 max-w-xl text-xs leading-5 text-muted-foreground">
          {supported
            ? "先启动服务，再用管理员账号进入控制台。服务由桌面端直接拉起，不用开终端。"
            : "输入反代服务的地址与账号，进入管理控制台。凭据不会保存在浏览器。"}
        </p>
      </header>

      <div className={cn("mt-7 grid items-start gap-5", supported && "lg:grid-cols-[minmax(0,1.05fr)_minmax(0,0.95fr)]")}>
        {supported ? <ServiceControl onServiceUp={onSuccess} /> : null}
        <LoginCard checking={checking} onSuccess={onSuccess} />
      </div>
    </div>
  );
}

/** 进控制台的三种凭据。服务端按请求体里出现的是 username / key / token 分流。 */
type LoginMode = "user" | "key" | "token";

function LoginCard({ checking, onSuccess }: { checking: boolean; onSuccess: () => void }) {
  const desktop = serviceSupported();
  const [base, setBase] = useState(getBaseUrl());
  const [mode, setMode] = useState<LoginMode>("user");
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");
  const [adminKey, setAdminKey] = useState("");
  const [apiToken, setApiToken] = useState("");
  const [options, setOptions] = useState<{ legacy_key: boolean; tokens: boolean } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [remember, setRemember] = useState(false);
  const restored = useRef(false);

  async function submit(override?: {
    mode: LoginMode; username: string; password: string; adminKey: string; apiToken: string; remember: boolean;
  }) {
    const target = override?.mode ?? mode;
    const user = (override?.username ?? username).trim();
    const pass = override?.password ?? password;
    const key = (override?.adminKey ?? adminKey).trim();
    const token = (override?.apiToken ?? apiToken).trim();
    const keep = override?.remember ?? remember;

    setBusy(true);
    setError("");
    setBaseUrl(base);
    try {
      const result =
        target === "key" ? await rp.loginKey(key)
          : target === "token" ? await rp.loginToken(token)
            : await rp.login(user, pass);
      setToken(result.token);
      // 登录成功才写凭据；没勾选则顺手清掉旧文件（「取消记住」要立刻生效）。
      // 管理令牌不动那个文件：它是长期有效的凭据，不该顶掉用户存好的账号口令。
      if (target !== "token") {
        try {
          await saveLogin(keep ? (target === "key" ? { mode: "key", key } : { mode: "user", username: user, password: pass }) : null);
        } catch (cause) {
          toast.error(cause instanceof Error ? cause.message : String(cause));
        }
      }
      onSuccess();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      // 自动读出来的口令已经失效（服务换过口令 / 换过服务）→ 清掉，
      // 免得每次进这个页面都白试一次。
      if (override?.remember && cause instanceof RpError && cause.status === 401) {
        setRemember(false);
        void saveLogin(null).catch(() => undefined);
      }
    } finally {
      setBusy(false);
    }
  }

  // 服务端最清楚哪些入口在当前部署下真的能用，就按它说的摆。
  // 拉不到（服务端版本较老、地址暂时不通）时一律退回「都摆出来」——
  // 一个探测失败不该把人挡在门外。
  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const result = await rp.loginOptions();
        if (alive) setOptions(result);
      } catch {
        /* 探测失败：保持默认，全部可用 */
      }
    })();
    return () => { alive = false; };
  }, []);

  const showKey = !options || options.legacy_key;
  const showToken = !options || options.tokens;
  // 当前选中的方式被服务端否掉就要退回账号密码，否则会停在一个不存在的表单上
  useEffect(() => {
    if (!options) return;
    if (mode === "key" && !options.legacy_key) setMode("user");
    if (mode === "token" && !options.tokens) setMode("user");
  }, [options, mode]);

  // 桌面端：进页面时读一次记住的凭据，填好并直接登录。
  // 浏览器形态不存也不读——那里的凭据不该留在本机。
  useEffect(() => {
    if (!desktop || restored.current) return;
    restored.current = true;
    // 刚点过「退出登录」就不自动登录。
    //
    // 退出会把界面切回登录页，
    // 而登录卡挂载时会读回保存的凭据并自动提交 —— 于是退出成功后立刻又登进去，
    // 用户看到的就是「点了没反应」。标记由 `performSignOut` 置位，活到本次进程结束；
    // 判定与重置都在 `lib/sign-out.ts`（那里有单测覆盖）。
    if (!shouldAutoLogin()) {
      setRemember(false);
      return;
    }
    void (async () => {
      let saved: RpSavedLogin | null = null;
      try {
        saved = await savedLogin();
      } catch {
        return;
      }
      if (!saved) return;
      setRemember(true);
      if (saved.mode === "key" && saved.key) {
        setMode("key");
        setAdminKey(saved.key);
        void submit({ mode: "key", username: "admin", password: "", adminKey: saved.key, apiToken: "", remember: true });
      } else if (saved.mode === "user" && saved.password) {
        const user = saved.username || "admin";
        setMode("user");
        setUsername(user);
        setPassword(saved.password);
        void submit({ mode: "user", username: user, password: saved.password, adminKey: "", apiToken: "", remember: true });
      }
    })();
    // submit / 各 setter 都是稳定引用或纯 setState，这里只在挂载时跑一次。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [desktop]);

  const modeTabs: { id: LoginMode; label: string }[] = [
    { id: "user", label: "账号密码" },
    ...(showKey ? [{ id: "key" as LoginMode, label: "管理密钥" }] : []),
    ...(showToken ? [{ id: "token" as LoginMode, label: "管理令牌" }] : []),
  ];

  return (
    <section className="min-w-0 rounded-xl border border-border/70 bg-card">
      <header className="border-b border-border/70 px-4 py-3">
        <h2 className="text-sm font-medium">进入控制台</h2>
        <p className="mt-0.5 text-[11px] text-muted-foreground">
          用户名 + 口令，或复用已有的管理密钥 / 管理令牌。
        </p>
      </header>

      <div className="flex min-w-0 flex-col gap-3 px-4 py-4">
        {/* 只剩一种方式时不摆切换按钮：一个孤零零的「账号密码」只会让人以为还能切 */}
        {modeTabs.length > 1 ? (
          <div className="flex flex-wrap gap-2">
            {modeTabs.map((item) => (
              <Button
                key={item.id}
                type="button"
                size="sm"
                variant={mode === item.id ? "default" : "outline"}
                onClick={() => setMode(item.id)}
              >
                {item.label}
              </Button>
            ))}
          </div>
        ) : null}

        {mode === "key" ? (
          <label className="flex flex-col gap-1.5">
            <span className="text-xs font-medium text-foreground/90">管理密钥</span>
            <Input type="password" value={adminKey} placeholder="ADMIN_KEY" onChange={(e) => setAdminKey(e.target.value)} />
            <span className="text-[11px] leading-5 text-muted-foreground">
              部署时通过 <code className="font-mono">ADMIN_KEY</code> 配置的单密钥入口，适合直接跑服务的场景。
            </span>
          </label>
        ) : mode === "token" ? (
          <label className="flex flex-col gap-1.5">
            <span className="text-xs font-medium text-foreground/90">管理令牌</span>
            <Input type="password" value={apiToken} placeholder="wbt_…" onChange={(e) => setApiToken(e.target.value)} />
            <span className="text-[11px] leading-5 text-muted-foreground">
              在「管理令牌」里生成。只读令牌进得来、看得到，但改不了东西；
              改用户和改令牌这两类操作仍然只认账号登录。
            </span>
          </label>
        ) : (
          <>
            <label className="flex flex-col gap-1.5">
              <span className="text-xs font-medium text-foreground/90">用户名</span>
              <Input value={username} onChange={(e) => setUsername(e.target.value)} />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-xs font-medium text-foreground/90">密码</span>
              <Input
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") void submit(); }}
              />
            </label>
          </>
        )}

        <label className="flex flex-col gap-1.5">
          <span className="text-xs font-medium text-foreground/90">服务地址</span>
          <Input value={base} onChange={(e) => setBase(e.target.value)} placeholder="http://127.0.0.1:8787" />
        </label>

        {error ? <Banner tone="bad">{error}</Banner> : null}

        {desktop && mode !== "token" ? (
          <label className="flex items-start gap-2 text-[11px] leading-5 text-muted-foreground">
            <input
              type="checkbox"
              className="mt-0.5 size-3.5 shrink-0 accent-primary"
              checked={remember}
              onChange={(event) => setRemember(event.target.checked)}
            />
            <span>
              保存密码，下次进这个页面直接登录。
              明文存在本机 <code className="font-mono">~/.wb-switch-plus/reverse-proxy-login.json</code>，
              取消勾选后下次登录成功即删除。
            </span>
          </label>
        ) : null}

        <Button
          size="sm"
          disabled={
            busy || checking ||
            (mode === "key" && !adminKey.trim()) ||
            (mode === "token" && !apiToken.trim()) ||
            (mode === "user" && (!username.trim() || !password))
          }
          onClick={() => void submit()}
        >
          {busy ? "正在连接…" : checking ? "检查会话…" : "进入控制台"}
        </Button>

        <p className="text-[11px] leading-5 text-muted-foreground">
          首次启动会创建 <code className="font-mono">admin</code> 账号，口令由服务生成并显示在服务卡片里；
          也可以用 <code className="font-mono">WB_ADMIN_PASSWORD</code> 自己指定。
        </p>
      </div>
    </section>
  );
}
