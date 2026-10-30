import { useState } from "react";
import { toast } from "sonner";
import { Copy, Plus } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { type RpToken } from "@/lib/reverse-proxy";
import { legacyRequest as rp } from "@/pages/reverse-proxy/legacy/legacy-admin";
import { Banner, CodeBlock, Empty, Field, PanelHeader, ToneBadge, fmtTime } from "./rp-shared";
import { ConfirmDialog, useConfirm } from "./confirm-dialog";

export function TokensPanel({ tokens, reload }: { tokens: RpToken[]; reload: () => Promise<void> }) {
  const [creating, setCreating] = useState(false);
  const [issued, setIssued] = useState<{ name: string; token: string } | null>(null);
  const confirm = useConfirm();

  function askDelete(token: RpToken) {
    confirm.ask({
      title: "删除这个管理令牌？",
      description: `使用「${token.name}」的自动化脚本将立即失效。`,
      confirmLabel: "删除",
      danger: true,
      run: () => rp(`tokens/${token.id}`, { method: "DELETE" }),
    });
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="管理令牌"
        desc="用于脚本 / CLI 访问本管理后台的令牌，按作用域授权。也能直接拿去登内置管理后台（服务地址 + /admin/），不必再记管理密钥。高危接口（用户、令牌、安全策略）只接受账号会话登录，令牌换来的会话同样进不去。"
        action={<Button size="sm" onClick={() => setCreating(true)}><Plus className="size-3.5" />创建令牌</Button>}
      />

      <Banner tone="warn">令牌只存哈希，明文仅在创建时显示一次；只读令牌不能修改任何数据。</Banner>

      {tokens.length === 0 ? (
        <Empty title="还没有管理令牌" desc="只读令牌适合监控脚本；管理员令牌适合自动化运维。" action={<Button size="sm" onClick={() => setCreating(true)}>创建令牌</Button>} />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-border/70">
          <table className="w-full min-w-[760px] text-sm">
            <thead className="bg-muted/40 text-xs text-muted-foreground">
              <tr>
                <th className="px-3 py-2 text-left font-medium">名称 / 标识</th>
                <th className="px-3 py-2 text-left font-medium">作用域</th>
                <th className="px-3 py-2 text-left font-medium">状态</th>
                <th className="px-3 py-2 text-left font-medium">创建 / 最近使用</th>
                <th className="px-3 py-2 text-right font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {tokens.map((token) => (
                <tr key={token.id} className="border-t border-border/60 align-top">
                  <td className="px-3 py-2.5">
                    <div className="font-medium">{token.name}</div>
                    <div className="font-mono text-[11px] text-muted-foreground">{token.prefix}…</div>
                  </td>
                  <td className="px-3 py-2.5">
                    <ToneBadge tone={token.scope === "admin" ? "warn" : "muted"}>{token.scope === "admin" ? "管理员" : "只读"}</ToneBadge>
                  </td>
                  <td className="px-3 py-2.5">
                    <ToneBadge tone={token.enabled ? "ok" : "muted"}>{token.enabled ? "启用" : "停用"}</ToneBadge>
                    <div className="mt-1 text-[11px] text-muted-foreground">{token.expires_at ? `至 ${fmtTime(token.expires_at)}` : "永不过期"}</div>
                  </td>
                  <td className="px-3 py-2.5 text-[11px] text-muted-foreground">
                    <div>创建 {fmtTime(token.created_at)}</div>
                    <div>最近 {token.last_used_at ? `${fmtTime(token.last_used_at)} · ${token.last_used_ip || "—"}` : "从未使用"}</div>
                    {token.created_by ? <div>by {token.created_by}</div> : null}
                  </td>
                  <td className="px-3 py-2.5">
                    <div className="flex justify-end gap-1.5">
                      <Button
                        variant="outline" size="sm" className="h-7 px-2 text-xs"
                        onClick={async () => {
                          try { await rp(`tokens/${token.id}`, { method: "PATCH", body: { enabled: !token.enabled } }); await reload(); }
                          catch (cause) { toast.error(cause instanceof Error ? cause.message : String(cause)); }
                        }}
                      >{token.enabled ? "停用" : "启用"}</Button>
                      <Button variant="outline" size="sm" className="h-7 px-2 text-xs text-destructive hover:text-destructive" onClick={() => askDelete(token)}>删除</Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      <CreateTokenDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={async (name, token) => { setCreating(false); await reload(); setIssued({ name, token }); }}
      />

      <Dialog open={Boolean(issued)} onOpenChange={(open) => { if (!open) setIssued(null); }}>
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>保存你的新令牌</DialogTitle>
            <DialogDescription>关闭后无法再次查看完整内容。它可以直接粘进内置管理后台的「管理令牌」入口登录。</DialogDescription>
          </DialogHeader>
          <CodeBlock action={
            <Button variant="outline" size="sm" onClick={() => { void navigator.clipboard.writeText(issued?.token ?? "").then(() => toast.success("已复制")).catch(() => toast.error("无法自动复制")); }}>
              <Copy className="size-3.5" />复制
            </Button>
          }>
            <span className="whitespace-pre-wrap break-all">{issued?.token}</span>
          </CodeBlock>
          <DialogFooter><Button size="sm" onClick={() => setIssued(null)}>我已保存</Button></DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog request={confirm.request} onClose={confirm.close} onDone={() => void reload().catch(() => undefined)} />
    </div>
  );
}

function CreateTokenDialog({ open, onOpenChange, onCreated }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (name: string, token: string) => Promise<void> | void;
}) {
  const [name, setName] = useState("");
  const [scope, setScope] = useState("readonly");
  const [days, setDays] = useState("0");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  async function submit() {
    if (!name.trim()) { setError("令牌名称不能只有空格"); return; }
    setBusy(true);
    setError("");
    try {
      const d = Number(days);
      const expires = d > 0 ? Math.floor(Date.now() / 1000) + Math.round(d * 86400) : null;
      const result = await rp<{ id: string; name: string; token: string }>("tokens", { method: "POST", body: { name: name.trim(), scope, expires_at: expires } });
      setName("");
      setDays("0");
      await onCreated(result.name, result.token);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!busy) onOpenChange(next); }}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>创建管理令牌</DialogTitle>
          <DialogDescription>只读令牌可读取概览等数据；管理员令牌可执行账号与密钥管理（但仍不能管理令牌本身）。</DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <Field label="名称"><Input value={name} maxLength={64} placeholder="例如：监控脚本" onChange={(e) => setName(e.target.value)} /></Field>
          <Field label="作用域">
            <Select value={scope} onValueChange={setScope}>
              <SelectTrigger className="h-9 text-sm"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="readonly">只读</SelectItem>
                <SelectItem value="admin">管理员</SelectItem>
              </SelectContent>
            </Select>
          </Field>
          <Field label="有效期（天）" hint="0 = 永不过期。">
            <Input type="number" min={0} value={days} onChange={(e) => setDays(e.target.value)} />
          </Field>
          {error ? <Banner tone="bad">{error}</Banner> : null}
        </div>
        <DialogFooter>
          <Button variant="outline" size="sm" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button size="sm" disabled={busy} onClick={() => void submit()}>{busy ? "创建中…" : "创建"}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
