import { toast } from "sonner";
import { RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Banner, CodeBlock, CopyButton, PanelHeader, StatCard, ToneBadge, fmtNum, fmtRate, fmtTime } from "./rp-shared";
import { POOL_STATE_LABELS, type RpOverview } from "@/lib/reverse-proxy";

const OUTCOMES: Record<string, string> = {
  success: "完成",
  stream_error: "流式错误",
  interrupted: "未完整结束",
  http_error: "请求失败",
};

export function OverviewPanel({ overview, onReload, onGoto, dataBase }: {
  overview: RpOverview;
  onReload: () => Promise<void>;
  onGoto: (tab: string) => void;
  /** 数据面地址（含 /v1）。内置页从 Layout 传，old 页传自己的 8788。 */
  dataBase?: string;
}) {
  const resolvedDataBase = dataBase ?? "";
  const { accounts, metrics: m, pool, events } = overview;
  const available = accounts.filter((a) => a.pool_state === "available").length;
  const known = accounts.filter((a) => typeof a.remaining === "number");

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="概览"
        desc="服务运行、账号积分与请求表现的即时快照。统计随服务重启清零。"
        action={
          <Button variant="outline" size="sm" onClick={() => void onReload().catch((e) => toast.error(e.message))}>
            <RefreshCw className="size-3.5" />刷新状态
          </Button>
        }
      />

      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        <StatCard label="可用账号" value={`${available} / ${accounts.length}`} hint="启用且未冷却、未确认耗尽" />
        <StatCard
          label="账号总积分"
          value={known.length ? fmtNum(known.reduce((s, a) => s + (a.remaining ?? 0), 0)) : "—"}
          hint={known.length < accounts.length ? `已查询 ${known.length}/${accounts.length} 个账号` : "以最近一次上游查询为准"}
        />
        <StatCard label="对话请求" value={fmtNum(m.completed)} hint={`API ${m.api_count} · 后台测试 ${m.test_count}`} />
        <StatCard label="完成成功率" value={fmtRate(m.success_rate)} hint="排除流式错误与未完整结束的请求" />
      </div>

      <div className="flex flex-wrap items-center gap-x-5 gap-y-1 rounded-xl border border-border/70 bg-card px-4 py-3 text-xs text-muted-foreground">
        <span>HTTP 成功率 <b className="text-foreground">{fmtRate(m.http_success_rate)}</b></span>
        <span>失败 <b className="text-foreground">{m.failed}</b></span>
        <span>进行中 <b className="text-foreground">{m.in_flight}</b></span>
        <span>平均耗时 <b className="text-foreground">{m.avg_duration_ms === null ? "—" : `${fmtNum(m.avg_duration_ms, 0)} ms`}</b></span>
        <Button variant="ghost" size="sm" className="ml-auto" onClick={() => onGoto("accounts")}>管理账号</Button>
      </div>

      {/*
        * 列模板必须写成 minmax(0, …)——裸 `1.15fr` 的最小尺寸是 `auto`，
        * 栅格会被内容的最小内容宽度顶宽，撑出 <main> 的横向滚动条
        * （概览页比主区域宽约 65px，底部会出现横向滚动条）。
        */}
      <div className="grid gap-4 lg:grid-cols-[minmax(0,1.15fr)_minmax(0,1fr)]">
        <section className="min-w-0 rounded-xl border border-border/70 bg-card p-4">
          <div className="flex items-center justify-between gap-2">
            <div>
              <h3 className="text-sm font-medium">账号工作区</h3>
              <p className="mt-0.5 text-xs text-muted-foreground">
                {pool.routing === "round_robin" ? "可用账号自动轮转" : "手动指定账号模式"}
              </p>
            </div>
            <Button variant="ghost" size="sm" onClick={() => onGoto("accounts")}>去账号池</Button>
          </div>
          <div className="mt-3 flex flex-col gap-2">
            {accounts.slice(0, 8).map((a) => {
              const state = POOL_STATE_LABELS[a.pool_state ?? ""] ?? { label: "待查询", tone: "muted" as const };
              return (
                <div key={a.id} className="flex items-center gap-3 rounded-lg border border-border/60 px-3 py-2">
                  <span className="flex size-7 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-medium">
                    {a.name.slice(0, 1)}
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm">{a.name}</div>
                    <div className="truncate text-[11px] text-muted-foreground">{a.uid || a.nickname || "—"}</div>
                  </div>
                  <div className="text-right">
                    <div className="text-sm tabular-nums">{a.remaining === null || a.remaining === undefined ? "待查询" : fmtNum(a.remaining)}</div>
                    <div className="text-[11px] text-muted-foreground">积分</div>
                  </div>
                  <ToneBadge tone={state.tone}>{state.label}</ToneBadge>
                </div>
              );
            })}
            {accounts.length === 0 ? <p className="py-6 text-center text-xs text-muted-foreground">尚未添加账号，点击「添加账号」开始。</p> : null}
            {accounts.length > 8 ? <p className="text-xs text-muted-foreground">更多账号请前往账号池查看。</p> : null}
          </div>
        </section>

        <div className="flex min-w-0 flex-col gap-4">
          <section className="rounded-xl border border-primary/25 bg-primary/[0.06] p-4">
            <h3 className="text-sm font-medium">API 接入地址</h3>
            <p className="mt-0.5 text-xs text-muted-foreground">一个入口，连接你的 WorkBuddy 模型。</p>
            <div className="mt-3">
              <CodeBlock action={<CopyButton value={resolvedDataBase} label="复制" />}>{resolvedDataBase}</CodeBlock>
            </div>
            <Button variant="ghost" size="sm" className="mt-2 px-0" onClick={() => onGoto("guide")}>查看接入指南</Button>
          </section>

          <section className="rounded-xl border border-border/70 bg-card p-4">
            <h3 className="text-sm font-medium">最近请求</h3>
            <p className="mt-0.5 text-xs text-muted-foreground">最近 100 条，仅记录时间、来源、接口、状态与耗时。</p>
            {/* 请求路径可能很长：这里让它自己横向滚，别把整页顶宽。 */}
            <div className="mt-3 max-h-64 overflow-auto">
              <table className="w-full text-xs">
                <thead className="text-muted-foreground">
                  <tr className="border-b border-border/60">
                    <th className="py-1.5 text-left font-medium">时间</th>
                    <th className="py-1.5 text-left font-medium">来源 / 接口</th>
                    <th className="py-1.5 text-left font-medium">状态</th>
                    <th className="py-1.5 text-right font-medium">耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {m.recent.map((r, i) => (
                    <tr key={i} className="border-b border-border/40 last:border-0">
                      <td className="py-1.5 font-mono text-[10px] text-muted-foreground">{fmtTime(r.time)}</td>
                      <td className="py-1.5">
                        {r.source === "test" ? "后台测试" : "API"}
                        <span className="ml-1 font-mono text-[10px] text-muted-foreground">{r.path}</span>
                      </td>
                      <td className="py-1.5">
                        <ToneBadge tone={r.ok ? "ok" : "bad"}>{r.status ?? "—"}</ToneBadge>
                        <span className="ml-1 text-[10px] text-muted-foreground">{OUTCOMES[r.outcome] ?? "请求失败"}</span>
                      </td>
                      <td className="py-1.5 text-right font-mono">{fmtNum(r.duration_ms, 0)} ms</td>
                    </tr>
                  ))}
                  {m.recent.length === 0 ? (
                    <tr><td colSpan={4} className="py-4 text-center text-muted-foreground">尚无请求记录。发起调用或后台测试后这里会自动更新。</td></tr>
                  ) : null}
                </tbody>
              </table>
            </div>
          </section>
        </div>
      </div>

      <section className="rounded-xl border border-border/70 bg-card p-4">
        <h3 className="text-sm font-medium">本次运行的测试记录</h3>
        <p className="mt-0.5 text-xs text-muted-foreground">仅记录状态与耗时，不记录测试消息或回复。</p>
        <div className="mt-3 flex flex-col gap-1.5">
          {events.slice(0, 6).map((e, i) => (
            <div key={i} className="flex items-center gap-3 text-xs">
              <span className="font-mono text-[10px] text-muted-foreground">{fmtTime(e.time)}</span>
              <span className="font-medium">{e.model}</span>
              <ToneBadge tone={e.ok ? "ok" : "bad"}>{e.ok ? "成功" : "失败"}</ToneBadge>
              <span className="font-mono">{e.seconds}s</span>
            </div>
          ))}
          {events.length === 0 ? <p className="text-xs text-muted-foreground">暂无测试记录，可在「连接测试」发送第一条。</p> : null}
        </div>
      </section>

      {overview.uptime > 0 ? (
        <Banner>
          服务已运行 {fmtSpan(overview.runtime?.session ?? overview.uptime)}
          {overview.runtime ? <>，累计运行 <strong>{fmtSpan(overview.runtime.total)}</strong></> : null}；
          统计随服务重启清零，不追溯历史请求（<b>运行时长</b>与<b>请求日志</b>会跨重启保留）。
        </Banner>
      ) : null}
    </div>
  );
}

/** 把秒数写成人话。比裸数字好读，也不用让用户自己算。 */
function fmtSpan(seconds: number): string {
  const s = Math.max(0, Math.floor(Number(seconds) || 0));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return `${h} 小时 ${m} 分钟`;
  if (m > 0) return `${m} 分钟`;
  return s < 60 ? "不到 1 分钟" : `${s} 秒`;
}
