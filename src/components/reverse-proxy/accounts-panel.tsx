import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import { toast } from "sonner";
import { Plus, QrCode, RefreshCw, Users } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { rp, POOL_STATE_LABELS, ROUTING_LABELS, type RpAccount, type RpCheckinLog, type RpOverview } from "@/lib/reverse-proxy";
import { isDesktop } from "@/lib/api";
import { Banner, Empty, Field, PanelHeader, StatCard, ToneBadge, fmtNum, fmtTime } from "./rp-shared";
import { ConfirmDialog, useConfirm } from "./confirm-dialog";
import { AccountLimitsDialog } from "./account-limits-dialog";

const FILTERS = [
  ["all", "全部状态"],
  ["available", "可用"],
  ["cooling", "冷却中"],
  ["paused", "已暂停"],
  ["exhausted", "积分耗尽"],
  ["invalid", "凭据异常"],
] as const;

/**
 * 行内限速角标：只有**真的偏离默认**才显示。
 *
 * 全池都设成同一个值时不该每行都挂个「5x」—— 那样等于没有信息量，
 * 而「这一行和别的行不一样」才是有用的信号。
 * 权重 0 单独写成「停转」：它是语义（不参与轮转），不是数量。
 */
function limitsBadge(a: RpAccount): string {
  const weight = Number(a.weight ?? 1);
  const concurrency = Number(a.max_concurrency ?? 0);
  const gap = Number(a.min_interval ?? 0);
  const parts: string[] = [];
  if (weight !== 1) parts.push(weight === 0 ? "停转" : `${weight}x`);
  if (concurrency > 0) parts.push(`≤${concurrency}`);
  if (gap > 0) parts.push(`${gap}s`);
  return parts.join(" ");
}

/** 与服务端 travel_label 一一对应（口径同桌面端 travel.rs 的 display_label）。 */
const TRAVEL_LABELS: Record<string, string> = {
  traveling: "旅行中", finished: "今日已派", "no-buddy": "无 Buddy", untraveled: "未旅行",
};

/** 到达时间上游有 ms / s 两种形态，统一成秒再交给 fmtTime。 */
function arriveSeconds(value?: number | null): number | null {
  if (!value) return null;
  return value > 1e12 ? Math.floor(value / 1000) : value;
}

/**
 * 旅行卡片的补充说明。返回 null 表示这一格不显示。
 *
 * 跨日的 no-buddy / finished 不再展示，否则账号卡片上会一直挂着一条昨天的状态；
 * 「旅行中」例外——跨日还没领到手的行程确实还在路上。
 */
function travelNote(a: RpAccount): string | null {
  const label = TRAVEL_LABELS[a.travel_label ?? ""];
  if (!label) return null;
  if (!a.travel_same_day && a.travel_label !== "traveling") return null;
  const parts = [label];
  if (a.travel_label === "traveling" && a.travel_arrive_at) parts.push(`预计 ${fmtTime(arriveSeconds(a.travel_arrive_at))}`);
  if (a.travel_location) parts.push(a.travel_location);
  if (a.travel_reward) parts.push(`+${a.travel_reward}`);
  return parts.join(" · ");
}

/**
 * 活跃上报的补充说明。返回 null 表示这一格不显示。
 *
 * 三档，因为「报上去了」和「真的生效了」是两件事：
 *   · active_today  —— 有正面证据（回读到的连登天数 > 0），今天确实生效了；
 *   · active_skip   —— 当天有效的终局跳过（国际版账号），今天不会再试；
 *   · active_error  —— 报过但没能证明生效，仍在按 30 分钟节拍重试。
 *
 * 前两档与 travelNote 同一口径：跨日不展示——否则账号卡片上会一直挂着「昨天报过」，
 * 看起来像今天已经做完了。第三档也是当天语义：服务端成功时会清空 active_error，
 * 所以它非空就必然意味着「今天试过了、但还没被证明生效」。
 */
export function activeNote(a: RpAccount): string | null {
  if (a.active_today) {
    const parts = ["今日已活跃上报"];
    // 必须是 > 0 的天数。0 不是「连登 0 天」，它是「这个事件此时此刻没生效」的
    // 读数；渲染成「连登 0 天」会把「本来有 9 天、今天被丢了」显示成「一天都没有」，
    // 那比不显示更误导。
    if (typeof a.active_streak === "number" && a.active_streak > 0) {
      parts.push(`连登 ${a.active_streak} 天`);
    }
    return parts.join(" · ");
  }
  if (a.active_skip) return "今日跳过活跃上报";
  if (a.active_error) return "活跃上报待确认";
  return null;
}

/** 把秒数说成人话：「4 分 30 秒」/「2 小时 5 分」/「3 天」。
 *
 * 上游给的是秒，界面直接摆「5400」没人算得出来是多久。超过一天就只说「N 天」——
 * 那时候精确到分已经没有意义了，反而让文案变长。 */
export function fmtLeft(sec: number | null | undefined): string {
  const n = Number(sec);
  if (!Number.isFinite(n) || n <= 0) return "";
  if (n >= 86400) return `${Math.floor(n / 86400)} 天`;
  if (n >= 3600) {
    const h = Math.floor(n / 3600);
    const m = Math.floor((n % 3600) / 60);
    return m ? `${h} 小时 ${m} 分` : `${h} 小时`;
  }
  if (n >= 60) {
    const m = Math.floor(n / 60);
    const s = Math.round(n % 60);
    return s ? `${m} 分 ${s} 秒` : `${m} 分`;
  }
  return `${Math.round(n)} 秒`;
}

/**
 * 上游 workbuddy2api 的权威冷却（模型级）。
 *
 * 为什么不并进上面那条「至 HH:MM」：两者来源完全不同。上面那条是我们**自己按
 * 错误码猜的**（整个账号一个时间戳、没有模型维度）；这条是上游状态机给的
 * （哪个模型、各自几点恢复、为什么）。混在一行，用户就分不清该等还是该去查。
 *
 * `up_cooling` 为 null/undefined 表示上游读不到或没配。
 * 这时绝不能画成「没有冷却」（那是替上游下结论 —— 它是「未知」，不是「健康」），
 * 但也不能什么都不画：那样「上游挂了，状态未知」和「上游说真的没限流」
 * 在界面上完全一样，用户不知道该等还是该换个模型 —— 这正是用户报的现象。
 *
 * 所以这里再读一次 `pool.wb2api_error`（上游最近一次读取失败的原因）：
 *   有错 + up_cooling 为 null  -> 「读不到，状态未知」
 *   无错 + up_cooling 为 null  -> 上游压根没配，这一块确实不该出现
 * 三种状态就此两两可分：未知 / 无冷却 / 有冷却。
 */
/**
 * 「未知」和「有冷却」用不同颜色：两者都是「暂时不能用」，
 * 但一个是我们没读到、一个是上游明确说了。
 * 同色会让人以为信息同样可靠，而前者其实只是「不知道」。
 */
function upstreamTone(a: RpAccount): string {
  const base = "text-[11px]";
  if (a.up_cooling === null || a.up_cooling === undefined)
    return `${base} text-muted-foreground`;
  return `${base} text-amber-600 dark:text-amber-400`;
}


export function upstreamNote(a: RpAccount, wb2apiError?: string | null): string | null {
  if (a.up_cooling === null || a.up_cooling === undefined) {
    // 只有「确实配了上游、但读不到」才说这句话。
    // 没配时不显示 —— 那是用户自己关掉的功能，不是故障。
    if (!wb2apiError) return null;
    return "上游读不到，冷却状态未知";
  }
  const models = Array.isArray(a.up_models) ? a.up_models : [];
  // 台账里的原因分两类，处置方式完全相反：
  //   6004       → 这个模型被限流，等一会儿就好
  //   11102 开头 → 这个号没开通这个模型，等多久都不会好，该换模型
  const limited: string[] = [];
  const missing: string[] = [];
  for (const m of models) {
    if (!m.model) continue;
    const label = m.remaining_sec ? `${m.model}（${fmtLeft(m.remaining_sec)}）` : m.model;
    ((m.reason ?? "").startsWith("11102") ? missing : limited).push(label);
  }
  const parts: string[] = [];
  if (a.up_cooling) {
    const left = fmtLeft(a.up_cool_remaining);
    parts.push(left ? `上游冷却中 · 还要 ${left}` : "上游冷却中");
  }
  if (limited.length) parts.push(`限流模型：${limited.join("、")}`);
  if (missing.length) parts.push(`未开通模型：${missing.join("、")}`);
  // 连败降权与限流退避都会表现为「冷却中」，但一个该等、一个该去查，必须分开说。
  if (a.up_degrade_until && a.up_degrade_until * 1000 > Date.now()) {
    parts.push(`连败降权 ${a.up_consecutive_fails ?? 0} 次`);
  }
  return parts.length ? parts.join(" · ") : null;
}

/** 任务流水类别。筛选项用 "all" 这个哨兵而不是空串——Radix 的 SelectItem 不接受空值。 */
const TASK_KINDS = [
  ["all", "全部类别"],
  ["checkin", "签到"],
  ["travel", "派猫猫旅行"],
  ["keepalive", "凭据保活"],
  ["active", "活跃上报"],
] as const;
const TASK_KIND_LABELS: Record<string, string> = { checkin: "签到", travel: "派猫猫旅行", keepalive: "凭据保活", active: "活跃上报" };

export function AccountsPanel({ overview, reload, onAddAccount, onBatchScan }: {
  overview: RpOverview;
  reload: () => Promise<void>;
  onAddAccount: () => void;
  /** 批量扫码入口。做成回调而不是内置弹窗：这个面板要能被别处复用而不带上半屏逻辑。 */
  onBatchScan: () => void;
}) {
  const { accounts, pool } = overview;
  const desktop = isDesktop();
  // 上游最近一次读取失败的说明（空 = 没问题）。它决定「冷却状态未知」那一条
  // 要不要出现 —— 见 upstreamNote。取在这里而不是每行各取一次：整批共用一个值。
  const wb2apiError = pool.wb2api_error ?? "";
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<string>("all");
  const [batchBusy, setBatchBusy] = useState(false);
  const [batchResult, setBatchResult] = useState("");
  const [renameTarget, setRenameTarget] = useState<RpAccount | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [settings, setSettings] = useState({
    routing: (pool.routing ?? "round_robin") as string,
    auto_checkin: Boolean(pool.auto_checkin),
    checkin_time: pool.checkin_time ?? "09:00",
    // 派猫猫旅行会对账号发起真实写请求，服务端默认关闭，这里也照着默认关。
    auto_travel: Boolean(pool.auto_travel),
    travel_time: pool.travel_time ?? "08:30",
    auto_keepalive: pool.auto_keepalive !== false,
    keepalive_hours: pool.keepalive_hours ?? 6,
    // 活跃上报同样会对上游发真实写请求，服务端默认关闭，这里照着默认关。
    auto_active: Boolean(pool.auto_active),
    active_time: pool.active_time ?? "10:00",
    active_reports: pool.active_reports ?? 1,
    // 上游 workbuddy2api 接入点。用 ?? "" 而不是 || ：空串是有效状态
    // （= 不读上游），用 || 会让它退成默认值，用户就清不掉已填的地址了。
    wb2api_base: pool.wb2api_base ?? "",
    wb2api_key: pool.wb2api_key ?? "",
    spread: Boolean(pool.spread),
    spread_minutes: pool.spread_minutes ?? 30,
    // 流量整形默认值。用 ?? 而不是 || —— 0 是有效取值（不限制/不限速），
    // 用 || 会把 0 变成另一个数的分支，用户清成 0 后一刷新又跳回来。
    default_weight: pool.default_weight ?? 1,
    default_max_concurrency: pool.default_max_concurrency ?? 0,
    default_min_interval: pool.default_min_interval ?? 0,
    // 全池总量闸门，0 = 不限制（缺省即不限制，所以 `?? 0` 与语义一致）。
    max_total_concurrency: pool.max_total_concurrency ?? 0,
  });
  const [savingSettings, setSavingSettings] = useState(false);
  /** 正在调限速的那个账号；null = 弹窗关闭。 */
  const [limitsTarget, setLimitsTarget] = useState<RpAccount | null>(null);
  const [taskLogs, setTaskLogs] = useState<RpCheckinLog[] | null>(null);
  const [logKind, setLogKind] = useState<string>("all");
  const confirm = useConfirm();

  /**
   * 任务流水。类别筛选交给服务端：流水是落盘的（上限 500 条），前端只取最近一页，
   * 在本地过滤会把「被别的类别挤出这一页」的记录彻底漏掉。
   */
  async function loadTaskLogs(kind: string = logKind) {
    try {
      setTaskLogs(await rp.checkinLogs(100, kind === "all" ? undefined : kind));
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    }
  }

  useEffect(() => {
    void loadTaskLogs("all");
    // 只在挂载时拉一次；之后由账号动作与筛选变化触发。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return accounts.filter((a) => {
      const matches = !q || [a.name, a.nickname, a.uid].filter(Boolean).join(" ").toLowerCase().includes(q);
      return matches && (filter === "all" || a.pool_state === filter);
    });
  }, [accounts, query, filter]);

  const counts = useMemo(() => {
    const by = (state: string) => accounts.filter((a) => a.pool_state === state).length;
    return { available: by("available"), cooling: by("cooling"), paused: by("paused"), exhausted: by("exhausted") };
  }, [accounts]);

  const known = accounts.filter((a) => typeof a.remaining === "number");
  const active = accounts.find((a) => a.active && a.enabled);

  async function runBatch(action: "status" | "checkin" | "travel" | "active") {
    setBatchBusy(true);
    setBatchResult("");
    try {
      const result = await rp.poolAction(action);
      setBatchResult(result.results.map((r) => `${accounts.find((a) => a.id === r.id)?.name ?? "账号"}：${r.message}`).join("；") || "没有启用的账号");
      await reload();
      await loadTaskLogs();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBatchBusy(false);
    }
  }

  async function runAccountAction(a: RpAccount, action: "status" | "checkin" | "refresh" | "travel" | "travel-claim" | "active") {
    try {
      const result = await rp.accountAction(a.id, action);
      toast[result.ok ? "success" : "error"](result.message);
      await reload();
      // 动作会往流水里写一条，顺手把列表拉一遍——否则要点一下刷新才看得到。
      await loadTaskLogs();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    }
  }

  function askToggle(a: RpAccount) {
    confirm.ask({
      title: a.enabled ? "暂停这个账号？" : "恢复这个账号？",
      description: "暂停后不参与新请求分配与自动签到；已开始的请求继续完成。",
      confirmLabel: a.enabled ? "暂停" : "恢复",
      danger: a.enabled,
      run: () => rp.editAccount(a.id, { enabled: !a.enabled }),
    });
  }

  function askDelete(a: RpAccount) {
    confirm.ask({
      title: "从本服务移除这个账号？",
      description: `「${a.name}」将不再参与本服务的调度与签到。账号库（workbuddy-switch）里的记录会保留——要彻底删除请到 switch 的账号管理页操作。`,
      confirmLabel: "移除",
      danger: true,
      run: () => rp.deleteAccount(a.id),
    });
  }

  function askActivate(a: RpAccount) {
    confirm.ask({
      title: "设为手动 / 测试账号？",
      description: `设为「${a.name}」。仅在手动调度模式与连接测试中使用。`,
      confirmLabel: "设为当前",
      run: () => rp.editAccount(a.id, { active: true }),
    });
  }

  async function saveSettings() {
    setSavingSettings(true);
    try {
      await rp.poolSettings(settings);
      toast.success("账号池设置已保存");
      await reload();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSavingSettings(false);
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="账号池"
        desc="集中管理登录凭据、积分与可用状态，让请求自动分配到可用账号。"
        action={
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" disabled={batchBusy} onClick={() => void runBatch("status")}>
              <RefreshCw className="size-3.5" />查询全部积分
            </Button>
            <Button variant="outline" size="sm" disabled={batchBusy} onClick={() => void runBatch("checkin")}>批量签到</Button>
            <Button
              variant="outline"
              size="sm"
              disabled={batchBusy}
              onClick={() =>
                confirm.ask({
                  title: "批量派猫猫旅行？",
                  description: "会以每个启用账号的身份向成长中心发起真实派发请求；已在旅途中或今日已派的账号自动跳过。",
                  confirmLabel: "批量派发",
                  run: () => runBatch("travel"),
                })
              }
            >
              批量派旅行
            </Button>
            <Button
              id="batch-active"
              variant="outline"
              size="sm"
              disabled={batchBusy}
              onClick={() =>
                confirm.ask({
                  title: "批量活跃上报？",
                  description: "会以每个启用账号的身份向上游发送一条 chat_request_send 事件，每号每天一轮。它只影响 growth 连登与领养猫的前置对话量，不消耗积分。",
                  confirmLabel: "批量上报",
                  run: () => runBatch("active"),
                })
              }
            >
              批量活跃上报
            </Button>
            {/* 账号的增删与登录归账户管理（同一份账号库）；这里只留一个入口，
                免得两个页面各有一套「添加账号」流程，最后各自维护一份状态。 */}
            <Button variant="outline" size="sm" asChild>
              <Link to="/"><Users className="size-3.5" />账户管理</Link>
            </Button>
            {/* 桌面端的「添加账号」直接复用主项目的 OAuth 扫码（写同一份账号库）；
               批量扫码只有反代自己的服务端流程，Rust 引擎还没有，所以桌面端不渲染——
                摆一个点了必然失败的按钮比没有按钮更糟。 */}
            <Button size="sm" onClick={onAddAccount} title={desktop ? "走账户管理的 OAuth 扫码登录" : undefined}>
              <Plus className="size-3.5" />{desktop ? "扫码登录" : "添加账号"}
            </Button>
            {desktop ? null : (
              <Button size="sm" variant="outline" onClick={onBatchScan}><QrCode className="size-3.5" />批量扫码</Button>
            )}
          </div>
        }
      />

      {/* 单一账号来源：这里不维护第二份账号表，只维护「池」这一层属性（是否参与调度、
          冷却、积分、限速）。账号的增删与登录一律在账户管理里做，避免两个页面各说一套。 */}
      <Banner tone="muted">
        账号与「账户管理」共用同一份账号库：新增、删除、重新登录都在账户管理完成，这里会在数秒内自动同步。
        本页只管池内调度（是否参与轮换、冷却、积分与限速），不再单独保存账号。
      </Banner>


      <div className="grid grid-cols-2 gap-3 lg:grid-cols-3">
        <StatCard label="已添加账号" value={accounts.length} hint="凭据安全保存在本地服务" />
        <StatCard
          label="请求调度"
          value={ROUTING_LABELS[pool.routing ?? "round_robin"]?.label ?? pool.routing ?? "轮流使用"}
          hint={
            pool.routing === "manual"
              ? active ? `只使用「${active.name}」` : "尚未指定当前账号"
              : pool.spread
                ? `轮换中 · 每 ${pool.spread_minutes ?? 30} 分钟换一个号`
                : pool.routing === "least_used" ? "优先分配给累计请求最少的账号" : "请求轮流分配到可用账号"
          }
        />
        <StatCard label="已知总积分" value={known.length ? fmtNum(known.reduce((s, a) => s + (a.remaining ?? 0), 0)) : "待查询"} hint={`全部 ${accounts.length} · 可用 ${counts.available} · 冷却 ${counts.cooling} · 暂停 ${counts.paused} · 耗尽 ${counts.exhausted}`} accent />
      </div>

      {batchResult ? <Banner tone="muted">{batchResult}</Banner> : null}

      <div className="flex flex-wrap items-center gap-2">
        <Input value={query} placeholder="搜索备注、昵称或 UID" className="h-8 max-w-64 text-xs" onChange={(e) => setQuery(e.target.value)} />
        <Select value={filter} onValueChange={setFilter}>
          <SelectTrigger className="h-8 w-36 text-xs"><SelectValue /></SelectTrigger>
          <SelectContent>
            {FILTERS.map(([value, label]) => <SelectItem key={value} value={value}>{label}</SelectItem>)}
          </SelectContent>
        </Select>
        <span className="ml-auto text-xs text-muted-foreground">共 {rows.length} 个</span>
      </div>

      {accounts.length === 0 ? (
        <Empty title="从第一个账号开始" desc="浏览器授权或导入登录文件，即可加入账号池。" action={<Button size="sm" onClick={onAddAccount}><Plus className="size-3.5" />添加登录凭据</Button>} />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-border/70">
          <table className="w-full min-w-[820px] text-sm">
            <thead className="bg-muted/40 text-xs text-muted-foreground">
              <tr>
                <th className="px-3 py-2 text-left font-medium">账号</th>
                <th className="px-3 py-2 text-left font-medium">状态 / 签到</th>
                <th className="px-3 py-2 text-left font-medium">剩余积分</th>
                <th className="px-3 py-2 text-left font-medium">凭据有效期</th>
                <th className="px-3 py-2 text-right font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((a) => {
                const state = POOL_STATE_LABELS[a.pool_state ?? ""] ?? { label: "待查询", tone: "muted" as const };
                const cooling = (a.cooldown_until ?? 0) > Date.now() / 1000;
                return (
                  <tr key={a.id} className="border-t border-border/60 align-top">
                    <td className="px-3 py-2.5">
                      <div className="flex items-start gap-2">
                        <span className="mt-0.5 flex size-7 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">{a.name.slice(0, 1)}</span>
                        <div className="min-w-0">
                          <div className="flex items-center gap-1.5">
                            <span className="truncate font-medium">{a.name}</span>
                            {a.active ? <ToneBadge tone="muted">手动 / 测试</ToneBadge> : null}
                            {/* 两版都标。只标国际版时，空白那一边到底是国内版还是
                                没读到 realm，界面上看不出区别，排查会白绕一圈。 */}
                            <ToneBadge tone="muted">
                              {a.realm === "ai" || a.realm === "global" ? "国际版" : "国内版"}
                            </ToneBadge>
                          </div>
                          <div className="truncate text-[11px] text-muted-foreground">{a.uid || a.nickname || "—"}</div>
                        </div>
                      </div>
                    </td>
                    <td className="px-3 py-2.5">
                      <ToneBadge tone={state.tone}>{state.label}</ToneBadge>
                      {/* 「凭据异常」有四种来源，处置方式完全不同，必须分开说：
                          桌面端标记需重新登录 / 上游确认失效 / 账号库里没有 token / 读不出来。 */}
                      {a.needs_relogin ? (
                        <div className="mt-1 text-[11px] text-destructive">
                          需重新登录{a.needs_relogin_reason ? `（${a.needs_relogin_reason}）` : ""}
                        </div>
                      ) : a.auth_invalid ? (
                        <div className="mt-1 text-[11px] text-destructive">凭据已被上游拒绝 · 先刷新再试</div>
                      ) : a.has_token === false ? (
                        <div className="mt-1 text-[11px] text-destructive">账号库里没有可用 token</div>
                      ) : null}
                      <div className="mt-1 text-[11px] text-muted-foreground">
                        {a.checkin_supported === false
                          ? "该版本无签到"
                          : a.today_checked_in ? "今日已签到" : "今日未确认签到"}
                      </div>
                      {travelNote(a) ? <div className="text-[11px] text-muted-foreground">{travelNote(a)}</div> : null}
                      {activeNote(a) ? <div className="text-[11px] text-muted-foreground">{activeNote(a)}</div> : null}
                      {/* 失败原因单独按错误色显示：active_error 是活跃上报专用的，
                          不显示出来「一直没点亮」这个状态在界面上就完全看不见。 */}
                      {!a.active_today && a.active_error
                        ? <div className="text-[11px] text-destructive">{a.active_error}</div>
                        : null}
                      {cooling ? <div className="text-[11px] text-muted-foreground">至 {fmtTime(a.cooldown_until)}</div> : null}
                      {/* 上游的权威冷却（模型级）：来源与我们自己猜的 cooldown_until 不同，
                          所以用琥珀色单独一行，而不是并进上面那句。读不到上游时
                          upstreamNote 返回 null，这一行整个不出现。 */}
                      {upstreamNote(a, wb2apiError) ? <div className={upstreamTone(a)}>{upstreamNote(a, wb2apiError)}</div> : null}
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="tabular-nums font-medium">{a.remaining === null || a.remaining === undefined ? "—" : fmtNum(a.remaining)}</div>
                      <div className="text-[11px] text-muted-foreground">{a.credits_updated ? fmtTime(a.credits_updated) : "点击查询积分"}{a.credits_stale && a.credits_updated ? " · 待刷新" : ""}</div>
                      {a.last_error ? <div className="text-[11px] text-destructive">{a.last_error}</div> : null}
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="font-mono text-[11px]">{fmtTime(a.expires_at ? a.expires_at / 1000 : null)}</div>
                      <div className="text-[11px] text-muted-foreground">{a.expired ? "已到期 · 调用时尝试刷新" : a.refresh_available ? "支持自动刷新" : "无刷新凭据"}</div>
                    </td>
                    <td className="px-3 py-2.5">
                      {/* 账号行的操作已经排到 9 个（查询/签到/派旅行/领奖/刷新/设为当前/备注/暂停/删除）。
                          限死宽度会把按钮压成竖条，所以只给一个下限让它们整齐地折成两行。
                          按钮上两个 class 都不能省：whitespace-nowrap 管文字不从中间折断，
                          shrink-0 管按钮本身不被压窄——只写前者的话 flex 仍会把它压到比
                          文字窄，文字溢出被裁。 */}
                      <div className="flex min-w-[350px] flex-wrap justify-end gap-1.5">
                        {a.enabled ? (
                          <>
                            <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => void runAccountAction(a, "status")}>查询积分</Button>
                            {/* 国际版没有签到体系：按钮点了必然失败，留着就是个陷阱。 */}
                            {a.checkin_supported === false ? null : (
                              <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => void runAccountAction(a, "checkin")}>签到</Button>
                            )}
                            <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" title="按服务端状态机派一次猫猫旅行；已在旅途中会提示" onClick={() => void runAccountAction(a, "travel")}>派旅行</Button>
                            <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" title="回查官方状态，到点则领奖" onClick={() => void runAccountAction(a, "travel-claim")}>领奖</Button>
                            <Button data-action="active" variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" title="向上游发一条 chat_request_send 事件点亮 growth 连登；每号每天一轮" onClick={() => void runAccountAction(a, "active")}>活跃</Button>
                            <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => void runAccountAction(a, "refresh")}>刷新凭据</Button>
                          </>
                        ) : null}
                        {a.enabled && !a.active ? (
                          <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => askActivate(a)}>设为当前</Button>
                        ) : null}
                        <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => { setRenameTarget(a); setRenameValue(a.name); }}>备注</Button>
                        <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" title="设置权重 / 并发上限 / 最小间隔" onClick={() => setLimitsTarget(a)}>
                          限速
                          {limitsBadge(a) ? <span className="ml-1 rounded bg-amber-500/20 px-1 text-[10px] font-semibold text-amber-500">{limitsBadge(a)}</span> : null}
                        </Button>
                        <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs" onClick={() => askToggle(a)}>{a.enabled ? "暂停" : "恢复"}</Button>
                        <Button variant="outline" size="sm" className="h-7 shrink-0 whitespace-nowrap px-2 text-xs text-destructive hover:text-destructive" title="只从反代池移除，账号库里的记录保留" onClick={() => askDelete(a)}>移出池</Button>
                      </div>
                    </td>
                  </tr>
                );
              })}
              {rows.length === 0 ? <tr><td colSpan={5} className="px-3 py-6 text-center text-xs text-muted-foreground">没有符合筛选条件的账号。</td></tr> : null}
            </tbody>
          </table>
        </div>
      )}

      <section className="rounded-xl border border-border/70 bg-card p-4">
        <PanelHeader title="请求调度" desc="决定多个账号之间怎么分摊请求。只用一个号时，该号的调用量会持续累积——轮流使用能显著降低单号被风控盯上的概率。" />
        <div className="mt-3 grid gap-2 sm:grid-cols-3">
          {(["round_robin", "least_used", "manual"] as const).map((value) => {
            const item = ROUTING_LABELS[value];
            const selected = settings.routing === value;
            return (
              <button
                key={value}
                type="button"
                onClick={() => setSettings((s) => ({ ...s, routing: value }))}
                className={`flex flex-col gap-1 rounded-lg border p-3 text-left transition-colors ${
                  selected ? "border-primary bg-primary/5" : "border-border/70 hover:bg-muted/50"
                }`}
              >
                <span className="flex items-center gap-2 text-xs font-medium">
                  <span className={`size-3 shrink-0 rounded-full border-2 ${selected ? "border-primary bg-primary" : "border-muted-foreground/40"}`} />
                  {item.label}
                </span>
                <span className="text-[11px] leading-4 text-muted-foreground">{item.desc}</span>
              </button>
            );
          })}
        </div>

        {settings.routing === "manual" ? (
          <div className="mt-3">
            <Banner tone={active ? "muted" : "bad"}>
              {active
                ? `当前只会使用「${active.name}」这一个账号。它暂停、冷却或凭据失效时，请求会直接失败，不会自动切到其它账号。`
                : "手动模式还没有指定「当前账号」：请在下面表格里点某个账号的「设为当前」，否则所有请求都会失败。"}
            </Banner>
          </div>
        ) : null}

        <div className="mt-4 flex flex-wrap items-end gap-3 border-t border-border/60 pt-4">
          <label className="flex h-8 items-center gap-2 text-xs">
            <input
              type="checkbox"
              className="size-4 accent-[var(--primary)]"
              checked={settings.spread}
              onChange={(e) => setSettings((s) => ({ ...s, spread: e.target.checked }))}
            />
            轮换驻留（每个号用一段时间再换下一个）
          </label>
          <Field label="每次驻留时长（分钟）">
            <Input
              type="number" min={1} max={1440} disabled={!settings.spread}
              className="h-8 w-28 text-xs"
              value={settings.spread_minutes}
              onChange={(e) => setSettings((s) => ({ ...s, spread_minutes: Number(e.target.value) || 1 }))}
            />
          </Field>
        </div>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          默认关闭：请求逐个账号轮着分配。开启后，轮到某个账号就把它固定使用一段时间，
          到期再换下一个——适合希望「一段时间内行为更像同一个人在连续使用」的场景。
          {pool.lease?.account_id ? (
            <>
              {" "}当前驻留中：<span className="text-foreground">
                {accounts.find((a) => a.id === pool.lease?.account_id)?.name ?? pool.lease.account_id}
              </span>（至 {fmtTime(pool.lease.until)}）
            </>
          ) : null}
        </p>

        <div className="mt-4 flex flex-wrap items-end gap-3 border-t border-border/60 pt-4">
          <Field label="签到时间（北京时间）">
            <Input type="time" className="h-8 w-32 text-xs" value={settings.checkin_time} onChange={(e) => setSettings((s) => ({ ...s, checkin_time: e.target.value }))} />
          </Field>
          <label className="flex h-8 items-center gap-2 text-xs">
            <input type="checkbox" className="size-4 accent-[var(--primary)]" checked={settings.auto_checkin} onChange={(e) => setSettings((s) => ({ ...s, auto_checkin: e.target.checked }))} />
            每日自动签到
          </label>
          <Field label="派旅行时间（北京时间）">
            <Input type="time" className="h-8 w-32 text-xs" value={settings.travel_time} onChange={(e) => setSettings((s) => ({ ...s, travel_time: e.target.value }))} />
          </Field>
          <label className="flex h-8 items-center gap-2 text-xs">
            <input type="checkbox" className="size-4 accent-[var(--primary)]" checked={settings.auto_travel} onChange={(e) => setSettings((s) => ({ ...s, auto_travel: e.target.checked }))} />
            每日派猫猫旅行
          </label>
          <Field label="保活提前量（小时）">
            <Input
              type="number" min={1} max={168} className="h-8 w-24 text-xs"
              value={settings.keepalive_hours}
              onChange={(e) => setSettings((s) => ({ ...s, keepalive_hours: Number(e.target.value) || 1 }))}
            />
          </Field>
          <label className="flex h-8 items-center gap-2 text-xs">
            <input type="checkbox" className="size-4 accent-[var(--primary)]" checked={settings.auto_keepalive} onChange={(e) => setSettings((s) => ({ ...s, auto_keepalive: e.target.checked }))} />
            凭据临近过期自动刷新
          </label>
          <Field label="活跃上报时间（北京时间）">
            <Input id="active-time" type="time" className="h-8 w-32 text-xs" value={settings.active_time} onChange={(e) => setSettings((s) => ({ ...s, active_time: e.target.value }))} />
          </Field>
          <label className="flex h-8 items-center gap-2 text-xs">
            <input id="auto-active" type="checkbox" className="size-4 accent-[var(--primary)]" checked={settings.auto_active} onChange={(e) => setSettings((s) => ({ ...s, auto_active: e.target.checked }))} />
            每日对话活跃上报
          </label>
          <Field label="每号每天上报条数">
            <Input
              id="active-reports"
              type="number" min={1} max={5} className="h-8 w-24 text-xs"
              value={settings.active_reports}
              onChange={(e) => setSettings((s) => ({ ...s, active_reports: Number(e.target.value) || 1 }))}
            />
          </Field>
          <Field label="上游 workbuddy2api 地址">
            {/* 填了才去读上游的模型级冷却；留空 = 不读（本机默认并没有跑它）。
                placeholder 给的是上游/原项目用的默认地址，省得用户去翻源码。 */}
            <Input
              id="wb2api-base"
              type="url" placeholder="http://127.0.0.1:7863"
              className="h-8 w-52 text-xs"
              value={settings.wb2api_base}
              onChange={(e) => setSettings((s) => ({ ...s, wb2api_base: e.target.value }))}
            />
          </Field>
          <Field label="上游管理密钥">
            <Input
              id="wb2api-key"
              type="password" placeholder="上游未设鉴权则留空" autoComplete="off"
              className="h-8 w-44 text-xs"
              value={settings.wb2api_key}
              onChange={(e) => setSettings((s) => ({ ...s, wb2api_key: e.target.value }))}
            />
          </Field>
          <Button size="sm" disabled={savingSettings} onClick={() => void saveSettings()}>{savingSettings ? "保存中…" : "保存设置"}</Button>
        </div>
        {/* 账号级流量整形的默认值。放在「请求调度」里而不是单独的卡片，
            因为它和轮转策略是同一件事的两面：轮转决定「轮到谁」，
            这三个字段决定「轮到之后允许多猛」。 */}
        <div className="mt-4 flex flex-wrap items-end gap-3 border-t border-border/60 pt-4">
          <Field label="新号默认权重">
            <Input
              type="number" min={0.1} max={100} step={0.1}
              className="h-8 w-24 text-xs"
              value={settings.default_weight}
              onChange={(e) => setSettings((s) => ({ ...s, default_weight: Number(e.target.value) || 0.1 }))}
            />
          </Field>
          <Field label="新号默认并发上限">
            <Input
              type="number" min={0} max={64} step={1}
              className="h-8 w-24 text-xs"
              value={settings.default_max_concurrency}
              onChange={(e) => setSettings((s) => ({ ...s, default_max_concurrency: Math.max(0, Number(e.target.value) || 0) }))}
            />
          </Field>
          <Field label="新号默认最小间隔（秒）">
            <Input
              type="number" min={0} max={3600} step={0.5}
              className="h-8 w-24 text-xs"
              value={settings.default_min_interval}
              onChange={(e) => setSettings((s) => ({ ...s, default_min_interval: Math.max(0, Number(e.target.value) || 0) }))}
            />
          </Field>
          {/* 全池总量闸门。和上面三项不同：上面是「每个号自己的上限」，
              这一项是「所有号加起来同时打到上游多少个」。20 个号各配 3 并发
              时，单号看全都合规，上游看到的却是 60 个请求同时到。 */}
          <Field label="全池并发上限">
            <Input
              type="number" min={0} max={256} step={1}
              className="h-8 w-24 text-xs"
              value={settings.max_total_concurrency ?? 0}
              onChange={(e) => setSettings((s) => ({ ...s, max_total_concurrency: Math.min(256, Math.max(0, Math.trunc(Number(e.target.value) || 0))) }))}
            />
          </Field>
        </div>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          这三项是<strong className="font-medium">事前防封</strong>的主要手段：封号的直接诱因几乎都是单账号短时间被打太多。
          上面的值是新建账号的初值，也可以在表格每行的「限速」里单独调。并发上限与最小间隔填 0 表示不限制；
          权重填 0 表示不参与自动轮转（手动指定仍可用），所以这里的默认权重不接受 0。
        </p>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          <strong className="font-medium">全池并发上限</strong>限制的是同时打到上游的
          <strong className="font-medium">总</strong>请求数，不分是哪个账号发的，超出的排队等一会儿再发。
          账号级上限治不了「每个号都没超、但整体太猛」——20 个号各配 3 并发，单号看全都合规，
          上游看到的却是 60 个请求同一瞬间到达。填 0 表示不限制。
        </p>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          派猫猫旅行按服务端状态机走：派发 → 到点回查 → 领奖；官网「累了，明天再来吧」即当日结束，不再重试。
          每日名额没派成或遇到可重试的失败时，每 30 分钟补一轮；领奖对账每 15 分钟一次，且不受自动派发开关影响——
          已经派出去的行程总会把奖励领回来。
        </p>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          活跃上报每号每天一轮：发 1 条即可点亮 growth 连登，发 5 条补满「领养猫需 5 次对话」的门槛。
          上报后服务会回读一次连登天数自检——这个接口返回 200 不等于生效（body 里缺 userId 时上游会静默丢弃事件），
          连登天数没涨就是被丢了。
        </p>
        <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
          填了「上游 workbuddy2api 地址」后，账号卡片会显示上游状态机给出的**模型级**冷却：
          哪个模型还被限流、各自还要等多久、以及是「被限流」（6004，等一会儿就好）还是
          「这个号没开通该模型」（11102，等多久都不会好）。它和我们自己按错误码估的冷却
          是两回事，所以分行显示。读不到上游时这一块不显示——「读不到」不等于「没有冷却」。
        </p>
      </section>

      <section className="rounded-xl border border-border/70 bg-card p-4">
        <div className="mb-3 flex flex-wrap items-end justify-between gap-2">
          <div>
            <h3 className="text-sm font-medium">自动任务流水</h3>
            <p className="mt-1 text-[11px] leading-4 text-muted-foreground">
              签到 / 派猫猫旅行 / 凭据保活的执行记录，最多保留最近 500 条。跨日只重试「无 Buddy」这类可恢复的失败，
              「今日已派」与已完成不再重试。
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Select value={logKind} onValueChange={(value) => { setLogKind(value); void loadTaskLogs(value); }}>
              <SelectTrigger id="task-log-kind" className="h-8 w-36 text-xs"><SelectValue /></SelectTrigger>
              <SelectContent>
                {TASK_KINDS.map(([value, label]) => <SelectItem key={value} value={value}>{label}</SelectItem>)}
              </SelectContent>
            </Select>
            <Button variant="outline" size="sm" className="h-8 text-xs" onClick={() => void loadTaskLogs()}>刷新</Button>
            <Button
              variant="outline"
              size="sm"
              className="h-8 text-xs"
              onClick={() =>
                confirm.ask({
                  title: "清空任务流水？",
                  description: "只清空这份记录，不影响账号、积分与旅行状态。",
                  confirmLabel: "清空",
                  danger: true,
                  run: async () => {
                    await rp.clearCheckinLogs(logKind === "all" ? undefined : logKind);
                    await loadTaskLogs();
                  },
                })
              }
            >
              清空
            </Button>
          </div>
        </div>
        <div className="overflow-hidden rounded-lg border border-border/60">
          <table className="w-full text-xs">
            <thead className="bg-muted/40 text-muted-foreground">
              <tr>
                <th className="px-3 py-2 text-left font-medium">时间</th>
                <th className="px-3 py-2 text-left font-medium">账号</th>
                <th className="px-3 py-2 text-left font-medium">类别</th>
                <th className="px-3 py-2 text-left font-medium">结果</th>
                <th className="px-3 py-2 text-left font-medium">说明</th>
              </tr>
            </thead>
            <tbody>
              {(taskLogs ?? []).map((row, index) => (
                <tr key={`${row.time}-${row.id}-${index}`} className="border-t border-border/60">
                  <td className="px-3 py-2 font-mono text-[11px] text-muted-foreground">{fmtTime(row.time)}</td>
                  <td className="px-3 py-2">{row.name || row.id}</td>
                  <td className="px-3 py-2">{TASK_KIND_LABELS[row.kind ?? "checkin"] ?? row.kind ?? "签到"}</td>
                  <td className="px-3 py-2">
                    <ToneBadge tone={row.ok ? "ok" : "bad"}>{row.ok ? "成功" : "失败"}</ToneBadge>
                    <span className="ml-1.5 text-[11px] text-muted-foreground">{row.source === "auto" ? "自动" : "手动"}</span>
                  </td>
                  <td className="px-3 py-2">
                    {row.message}
                    {row.credits ? <span className="ml-1.5 text-[11px] text-emerald-600 dark:text-emerald-400">+{row.credits} 积分</span> : null}
                  </td>
                </tr>
              ))}
              {taskLogs === null ? (
                <tr><td colSpan={5} className="px-3 py-6 text-center text-xs text-muted-foreground">正在读取任务流水…</td></tr>
              ) : null}
              {taskLogs?.length === 0 ? (
                <tr><td colSpan={5} className="px-3 py-6 text-center text-xs text-muted-foreground">还没有任务记录。开启自动任务，或在账号列表点「签到 / 派旅行」。</td></tr>
              ) : null}
            </tbody>
          </table>
        </div>
      </section>

      <Banner>
        访问令牌过期时服务会尝试自动刷新；刷新凭据也失效后，请在桌面端重新登录并导入。"可刷新" 不代表已验证账号额度。
      </Banner>

      <Dialog open={Boolean(renameTarget)} onOpenChange={(open) => { if (!open) setRenameTarget(null); }}>
        <DialogContent className="max-w-sm">
          <DialogHeader><DialogTitle>编辑账号备注</DialogTitle></DialogHeader>
          <Label className="text-xs">账号备注</Label>
          <Input value={renameValue} maxLength={60} onChange={(e) => setRenameValue(e.target.value)} />
          <DialogFooter>
            <Button variant="outline" size="sm" onClick={() => setRenameTarget(null)}>取消</Button>
            <Button
              size="sm"
              onClick={async () => {
                if (!renameTarget) return;
                try {
                  await rp.editAccount(renameTarget.id, { name: renameValue });
                  setRenameTarget(null);
                  toast.success("备注已保存");
                  await reload();
                } catch (cause) {
                  toast.error(cause instanceof Error ? cause.message : String(cause));
                }
              }}
            >保存</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog request={confirm.request} onClose={confirm.close} onDone={() => void reload().catch(() => undefined)} />
      <AccountLimitsDialog
        account={limitsTarget}
        defaults={{
          weight: settings.default_weight,
          max_concurrency: settings.default_max_concurrency,
          min_interval: settings.default_min_interval,
        }}
        onClose={() => setLimitsTarget(null)}
        onSaved={() => reload()}
      />
    </div>
  );
}
