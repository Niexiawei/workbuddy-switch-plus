import { useState } from "react";
import { toast } from "sonner";
import { Plus, ShieldCheck } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { type RpSession, type RpUser } from "@/lib/reverse-proxy";
import { legacyRequest as rp } from "@/pages/reverse-proxy/legacy/legacy-admin";
import { Banner, Empty, Field, PanelHeader, ToneBadge } from "./rp-shared";
import { ConfirmDialog, useConfirm } from "./confirm-dialog";

export function UsersPanel({ users, session, reload, onRelogin }: {
  users: RpUser[];
  session: RpSession;
  reload: () => Promise<void>;
  onRelogin: () => void;
}) {
  const [creating, setCreating] = useState(false);
  const [passwordTarget, setPasswordTarget] = useState<RpUser | null>(null);
  const [newPassword, setNewPassword] = useState("");
  const confirm = useConfirm();

  async function setRole(user: RpUser, role: string) {
    try {
      const result = await rp<{ username: string; role: string; sessions_revoked?: boolean; relogin_required?: boolean }>(`users/${encodeURIComponent(user.username)}`, { method: "PATCH", body: { role } });
      toast.success(`已将 ${user.username} 设为${role === "admin" ? "管理员" : "只读"}`);
      if (result.relogin_required) { onRelogin(); return; }
      await reload();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : String(cause));
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="管理用户"
        desc="多用户登录：口令以 PBKDF2-SHA256 加盐哈希保存；改密码 / 改角色会立刻吊销该用户已登录的会话。"
        action={<Button size="sm" onClick={() => setCreating(true)}><Plus className="size-3.5" />新建用户</Button>}
      />

      <div className="flex flex-wrap items-center gap-3 rounded-xl border border-border/70 bg-card px-4 py-3 text-xs text-muted-foreground">
        <span className="inline-flex items-center gap-1.5 text-foreground">
          <ShieldCheck className="size-3.5" />当前登录：<b>{session.username}</b>
        </span>
        <ToneBadge tone={session.role === "admin" ? "warn" : "muted"}>{session.role === "admin" ? "管理员" : "只读"}</ToneBadge>
        <Button
          variant="outline" size="sm" className="ml-auto h-7 text-xs"
          onClick={() => confirm.ask({
            title: "吊销自己全部会话？",
            description: "所有已登录的浏览器会话（含当前这次）都会立即失效，需要重新登录。",
            confirmLabel: "吊销并重新登录",
            danger: true,
            run: async () => { await rp("sessions/revoke", { method: "POST" }); onRelogin(); },
          })}
        >吊销我的全部会话</Button>
      </div>

      {users.length === 0 ? (
        <Empty title="没有用户" desc="首次启动会自动创建 admin 用户。" />
      ) : (
        <div className="overflow-x-auto rounded-xl border border-border/70">
          <table className="w-full min-w-[560px] text-sm">
            <thead className="bg-muted/40 text-xs text-muted-foreground">
              <tr>
                <th className="px-3 py-2 text-left font-medium">用户名</th>
                <th className="px-3 py-2 text-left font-medium">角色</th>
                <th className="px-3 py-2 text-right font-medium">操作</th>
              </tr>
            </thead>
            <tbody>
              {users.map((user) => {
                const isSelf = user.username === session.username;
                return (
                  <tr key={user.username} className="border-t border-border/60">
                    <td className="px-3 py-2.5 font-medium">
                      {user.username}
                      {isSelf ? <span className="ml-1.5 text-[11px] text-muted-foreground">（我）</span> : null}
                    </td>
                    <td className="px-3 py-2.5">
                      <Select value={user.role} onValueChange={(value) => void setRole(user, value)}>
                        <SelectTrigger className="h-7 w-28 text-xs"><SelectValue /></SelectTrigger>
                        <SelectContent>
                          <SelectItem value="admin">管理员</SelectItem>
                          <SelectItem value="viewer">只读</SelectItem>
                        </SelectContent>
                      </Select>
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex justify-end gap-1.5">
                        <Button variant="outline" size="sm" className="h-7 px-2 text-xs" onClick={() => { setPasswordTarget(user); setNewPassword(""); }}>改密码</Button>
                        <Button
                          variant="outline" size="sm" className="h-7 px-2 text-xs text-destructive hover:text-destructive"
                          disabled={isSelf}
                          onClick={() => confirm.ask({
                            title: "删除这个用户？",
                            description: `「${user.username}」将无法再登录，其会话立即失效。`,
                            confirmLabel: "删除",
                            danger: true,
                            run: () => rp(`users/${encodeURIComponent(user.username)}`, { method: "DELETE" }),
                          })}
                        >删除</Button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <CreateUserDialog open={creating} onOpenChange={setCreating} onCreated={async () => { setCreating(false); await reload(); }} />

      <Dialog open={Boolean(passwordTarget)} onOpenChange={(open) => { if (!open) setPasswordTarget(null); }}>
        <DialogContent className="max-w-sm">
          <DialogHeader>
            <DialogTitle>修改密码</DialogTitle>
            <DialogDescription>「{passwordTarget?.username}」的既有会话将立即失效。</DialogDescription>
          </DialogHeader>
          <Field label="新密码" hint="至少 8 位。">
            <Input type="password" value={newPassword} onChange={(e) => setNewPassword(e.target.value)} />
          </Field>
          <DialogFooter>
            <Button variant="outline" size="sm" onClick={() => setPasswordTarget(null)}>取消</Button>
            <Button
              size="sm"
              onClick={async () => {
                if (!passwordTarget) return;
                try {
                  const result = await rp<{ username: string; role: string; sessions_revoked?: boolean; relogin_required?: boolean }>(`users/${encodeURIComponent(passwordTarget.username)}`, { method: "PATCH", body: { password: newPassword } });
                  toast.success("密码已更新");
                  setPasswordTarget(null);
                  if (result.relogin_required) { onRelogin(); return; }
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
    </div>
  );
}

function CreateUserDialog({ open, onOpenChange, onCreated }: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: () => Promise<void> | void;
}) {
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState("viewer");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!busy) onOpenChange(next); }}>
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>新建管理用户</DialogTitle>
          <DialogDescription>管理员可管理账号与密钥；只读用户仅能查看。</DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <Field label="用户名"><Input value={username} maxLength={64} onChange={(e) => setUsername(e.target.value)} /></Field>
          <Field label="密码" hint="至少 8 位。"><Input type="password" value={password} onChange={(e) => setPassword(e.target.value)} /></Field>
          <Field label="角色">
            <Select value={role} onValueChange={setRole}>
              <SelectTrigger className="h-9 text-sm"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="viewer">只读</SelectItem>
                <SelectItem value="admin">管理员</SelectItem>
              </SelectContent>
            </Select>
          </Field>
          {error ? <Banner tone="bad">{error}</Banner> : null}
        </div>
        <DialogFooter>
          <Button variant="outline" size="sm" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button
            size="sm" disabled={busy}
            onClick={async () => {
              setBusy(true); setError("");
              try {
                await rp("users", { method: "POST", body: { username: username.trim(), password, role } });
                setUsername(""); setPassword(""); setRole("viewer");
                toast.success("用户已创建");
                await onCreated();
              } catch (cause) {
                setError(cause instanceof Error ? cause.message : String(cause));
              } finally {
                setBusy(false);
              }
            }}
          >{busy ? "创建中…" : "创建"}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
