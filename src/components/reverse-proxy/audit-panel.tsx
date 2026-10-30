import { useCallback, useEffect, useState } from "react";
import { Loader2, RefreshCw, Trash2 } from "lucide-react";

import { Banner, Empty, PanelHeader, ToneBadge, fmtTime } from "@/components/reverse-proxy/rp-shared";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { type RpAuditLog } from "@/lib/reverse-proxy";
import { legacyRequest as rp } from "@/pages/reverse-proxy/legacy/legacy-admin";
import { ConfirmDialog, useConfirm } from "./confirm-dialog";

/**
 * 审计日志。
 *
 * `/admin/api/audit-logs` 从写出来那天起就没有任何界面调用过它（桌面端和网页端
 * 一次都没有）—— 也就是说每一条改动都老老实实记了，但没人看得到。
 * 出事之后第一个要问的恰恰是「这个号是谁、在什么时候删的」。
 *
 * 两个刻意的设计：
 *   · **只读**。审计日志不该被编辑；唯一允许的动作是清空，而清空本身会被记一条新记录
 *     （后端 `clear_audit_logs` 先清再写，所以清空后还能看到是谁清的）。
 *   · **不做分页**。接口最多回 200 条，这个量级一屏滚动就够；加分页反而让人以为
 *     自己看到的是全部历史。
 */
const PAGE_LIMIT = 200;

export function AuditPanel() {
  const [rows, setRows] = useState<RpAuditLog[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  /** 客户端过滤。审计日志按动作/操作者找比按时间翻快得多。 */
  const [query, setQuery] = useState("");
  const confirm = useConfirm();

  const load = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      setRows(await rp(`audit-logs?limit=${PAGE_LIMIT}`));
    } catch (cause) {
      // 只读身份（viewer）进不来这个接口 —— 那不是错误而是策略，
      // 所以文案要说清是权限，别让用户去猜是不是服务挂了。
      const text = cause instanceof Error ? cause.message : String(cause);
      setError(`审计日志读取失败：${text}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  const q = query.trim().toLowerCase();
  const shown = q
    ? rows.filter((row) =>
      [row.actor, row.action, row.target, row.detail]
        .some((field) => String(field ?? "").toLowerCase().includes(q)))
    : rows;

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="审计日志"
        desc="每一次改动类操作都会留一行：登录、改密钥、改账号、改安全策略、删分组、清空日志本身。这份记录只写不改，默认全量保留。"
        action={
          <div className="flex items-center gap-2">
            <Input
              className="h-8 w-52 text-xs"
              placeholder="按操作者 / 动作 / 对象筛"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            <Button size="sm" variant="outline" disabled={loading} onClick={() => void load()}>
              {loading ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}刷新
            </Button>
            <Button
              size="sm" variant="outline"
              className="text-destructive hover:text-destructive"
              disabled={rows.length === 0}
              onClick={() => confirm.ask({
                title: "清空审计日志？",
                description: "只清掉历史记录。清空这个动作本身会被记成一条新记录，所以清完之后这一页不会是空的。",
                confirmLabel: "清空",
                danger: true,
                run: () => rp("audit-logs/clear", { method: "POST" }),
              })}
            >
              <Trash2 className="size-3.5" />清空
            </Button>
          </div>
        }
      />

      {error ? <Banner tone="bad">{error}</Banner> : null}

      <p className="text-[11px] leading-4 text-muted-foreground">
        接口最多返回 <strong className="font-medium">{PAGE_LIMIT}</strong> 条（最新的在前）。
        {q ? `当前筛出 ${shown.length} / ${rows.length} 条。` : null}
        「详情」里的口令、密钥、令牌一律不会出现原文，只记字段名。
      </p>

      {shown.length === 0 ? (
        <Empty
          title={rows.length === 0 ? "还没有审计记录" : "没有匹配的记录"}
          desc={rows.length === 0 ? "从下一次改动开始就会出现在这里。" : "换个关键词试试。"}
        />
      ) : (
        <div className="max-h-[34rem] overflow-auto rounded-xl border border-border/70">
          <table className="w-full min-w-[860px] text-xs">
            <thead className="sticky top-0 bg-muted/60 text-muted-foreground backdrop-blur">
              <tr>
                <th className="px-3 py-2 text-left font-medium">时间</th>
                <th className="px-3 py-2 text-left font-medium">操作者</th>
                <th className="px-3 py-2 text-left font-medium">动作</th>
                <th className="px-3 py-2 text-left font-medium">对象</th>
                <th className="px-3 py-2 text-left font-medium">详情</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((row, index) => (
                <tr key={`${row.time}-${index}`} className="border-t border-border/50 align-top">
                  <td className="whitespace-nowrap px-3 py-1.5 font-mono text-[10px] text-muted-foreground">
                    {fmtTime(row.time)}
                  </td>
                  <td className="px-3 py-1.5">
                    <ToneBadge tone={row.actor === "system" ? "muted" : "ok"}>{row.actor || "—"}</ToneBadge>
                  </td>
                  <td className="px-3 py-1.5 font-mono text-[11px]">{row.action}</td>
                  <td className="px-3 py-1.5 font-mono text-[11px] text-muted-foreground">{row.target || "—"}</td>
                  <td className="px-3 py-1.5 text-[11px] text-muted-foreground">{row.detail || "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <ConfirmDialog request={confirm.request} onClose={confirm.close} onDone={() => void load().catch(() => undefined)} />
    </div>
  );
}
