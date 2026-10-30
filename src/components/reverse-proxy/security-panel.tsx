import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { Plus, RefreshCw } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { rp, type RpIpLog, type RpIpRule, type RpSecurityConfig } from "@/lib/reverse-proxy";
import { Banner, Empty, Field, PanelHeader, ToneBadge, fmtTime } from "./rp-shared";
import { ConfirmDialog, useConfirm } from "./confirm-dialog";

export function SecurityPanel() {
  const [config, setConfig] = useState<RpSecurityConfig>({ enabled: false, mode: "blacklist" });
  const [rules, setRules] = useState<RpIpRule[]>([]);
  const [logs, setLogs] = useState<RpIpLog[]>([]);
  const [kind, setKind] = useState("allow");
  const [cidr, setCidr] = useState("");
  const [note, setNote] = useState("");
  const [saving, setSaving] = useState(false);
  const confirm = useConfirm();

  const load = useCallback(async () => {
    try {
      const [c, r, l] = await Promise.all([rp.securityConfig(), rp.ipRules(), rp.ipLogs()]);
      setConfig(c); setRules(r); setLogs(l);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  async function saveConfig(next: RpSecurityConfig) {
    setSaving(true);
    try {
      setConfig(await rp.setSecurityConfig(next));
      toast.success("安全策略已保存");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="IP 安全"
        desc="对入站的 /v1 与 /admin 请求统一做 IP 白/黑名单管控；关闭时零开销。"
        action={<Button variant="outline" size="sm" onClick={() => void load()}><RefreshCw className="size-3.5" />刷新</Button>}
      />

      <section className="flex flex-wrap items-end gap-4 rounded-xl border border-border/70 bg-card p-4">
        <label className="flex items-center gap-2 text-sm">
          <Switch checked={config.enabled} disabled={saving} onCheckedChange={(checked) => void saveConfig({ ...config, enabled: checked })} />
          启用 IP 管控
        </label>
        <Field label="模式">
          <Select value={config.mode} onValueChange={(mode) => void saveConfig({ ...config, mode: mode as RpSecurityConfig["mode"] })}>
            <SelectTrigger className="h-8 w-40 text-xs"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="blacklist">黑名单（默认放行）</SelectItem>
              <SelectItem value="whitelist">白名单（默认拒绝）</SelectItem>
            </SelectContent>
          </Select>
        </Field>
        <p className="max-w-md text-[11px] leading-4 text-muted-foreground">
          黑名单：命中 deny 即拒绝。白名单：只有命中 allow 才放行——启用白名单前务必先加一条放行「127.0.0.1」，
          否则本机也会被挡在外面。
        </p>
      </section>

      {config.enabled && config.mode === "whitelist" && !rules.some((r) => r.kind === "allow") ? (
        <Banner tone="bad">当前是白名单模式且没有任何放行规则：所有请求（含本机）都会被拒绝。</Banner>
      ) : null}

      <Tabs defaultValue="rules">
        <TabsList>
          <TabsTrigger value="rules">规则（{rules.length}）</TabsTrigger>
          <TabsTrigger value="access">访问日志（{logs.length}）</TabsTrigger>
        </TabsList>

        <TabsContent value="rules" className="mt-4 flex flex-col gap-4">
          <div className="flex flex-wrap items-end gap-2 rounded-xl border border-border/70 bg-card p-4">
            <Field label="类型">
              <Select value={kind} onValueChange={setKind}>
                <SelectTrigger className="h-8 w-24 text-xs"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="allow">放行</SelectItem>
                  <SelectItem value="deny">拒绝</SelectItem>
                </SelectContent>
              </Select>
            </Field>
            <Field label="IP / CIDR"><Input value={cidr} placeholder="10.0.0.0/8" className="h-8 w-44 font-mono text-xs" onChange={(e) => setCidr(e.target.value)} /></Field>
            <Field label="备注"><Input value={note} maxLength={120} className="h-8 w-48 text-xs" onChange={(e) => setNote(e.target.value)} /></Field>
            <Button
              size="sm"
              onClick={async () => {
                try {
                  await rp.addIpRule(kind, cidr.trim(), note.trim());
                  setCidr(""); setNote("");
                  toast.success("规则已添加");
                  await load();
                } catch (cause) {
                  toast.error(cause instanceof Error ? cause.message : String(cause));
                }
              }}
              disabled={!cidr.trim()}
            ><Plus className="size-3.5" />添加</Button>
          </div>

          {rules.length === 0 ? <Empty title="暂无规则" desc="添加一条放行或拒绝规则。" /> : (
            <div className="overflow-hidden rounded-xl border border-border/70">
              <table className="w-full text-sm">
                <thead className="bg-muted/40 text-xs text-muted-foreground">
                  <tr>
                    <th className="px-3 py-2 text-left font-medium">类型</th>
                    <th className="px-3 py-2 text-left font-medium">IP / CIDR</th>
                    <th className="px-3 py-2 text-left font-medium">备注</th>
                    <th className="px-3 py-2 text-left font-medium">创建时间</th>
                    <th className="px-3 py-2 text-right font-medium">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {rules.map((rule) => (
                    <tr key={rule.id} className="border-t border-border/60">
                      <td className="px-3 py-2"><ToneBadge tone={rule.kind === "allow" ? "ok" : "bad"}>{rule.kind === "allow" ? "放行" : "拒绝"}</ToneBadge></td>
                      <td className="px-3 py-2 font-mono text-xs">{rule.cidr}</td>
                      <td className="px-3 py-2 text-xs text-muted-foreground">{rule.note || "—"}</td>
                      <td className="px-3 py-2 text-[11px] text-muted-foreground">{fmtTime(rule.created_at)}</td>
                      <td className="px-3 py-2 text-right">
                        <Button
                          variant="outline" size="sm" className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                          onClick={() => confirm.ask({
                            title: "删除这条规则？",
                            description: `${rule.kind === "allow" ? "放行" : "拒绝"} ${rule.cidr}`,
                            confirmLabel: "删除", danger: true,
                            run: async () => { await rp.deleteIpRule(rule.id); await load(); },
                          })}
                        >删除</Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </TabsContent>

        <TabsContent value="access" className="mt-4 flex flex-col gap-3">
          <div className="flex justify-end">
            <Button
              variant="outline" size="sm"
              onClick={() => confirm.ask({ title: "清空访问日志？", confirmLabel: "清空", danger: true, run: async () => { await rp.clearIpLogs(); await load(); } })}
            >清空日志</Button>
          </div>
          {logs.length === 0 ? <Empty title="暂无访问记录" desc="启用 IP 管控后，请求会被记录在此。" /> : (
            <div className="max-h-[28rem] overflow-y-auto rounded-xl border border-border/70">
              <table className="w-full text-xs">
                <thead className="sticky top-0 bg-muted/60 text-muted-foreground backdrop-blur">
                  <tr>
                    <th className="px-3 py-2 text-left font-medium">时间</th>
                    <th className="px-3 py-2 text-left font-medium">来源 IP</th>
                    <th className="px-3 py-2 text-left font-medium">接口</th>
                    <th className="px-3 py-2 text-left font-medium">结果</th>
                  </tr>
                </thead>
                <tbody>
                  {logs.map((log, i) => (
                    <tr key={i} className="border-t border-border/50">
                      <td className="px-3 py-1.5 font-mono text-[10px] text-muted-foreground">{fmtTime(log.ts)}</td>
                      <td className="px-3 py-1.5 font-mono">{log.ip}</td>
                      <td className="px-3 py-1.5 font-mono">{log.path}</td>
                      <td className="px-3 py-1.5"><ToneBadge tone={log.blocked ? "bad" : "ok"}>{log.blocked ? "拦截" : "放行"}</ToneBadge></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </TabsContent>
      </Tabs>

      <ConfirmDialog request={confirm.request} onClose={confirm.close} />
    </div>
  );
}