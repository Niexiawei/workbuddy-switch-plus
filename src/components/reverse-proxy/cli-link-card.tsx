import { useCallback, useEffect, useState } from "react";
import { toast } from "sonner";
import { Loader2, RefreshCw, RotateCw, Terminal, Unplug } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Banner, CopyButton, Field, PanelHeader, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import { disableCodebuddyCliProxy, enableCodebuddyCliProxy, getCodebuddyCliStatus } from "@/lib/api";
import { normalizeProxyBase } from "@/lib/cli-link";
import { rp } from "@/lib/reverse-proxy";
import { proxyServiceStatus } from "@/lib/proxy-service";
import type { CodeBuddyCliStatus } from "@/lib/types";

/**
 * CodeBuddy CLI 接入卡片。
 *
 * 开启后 CLI 的 `CODEBUDDY_BASE_URL` 指向本机反代、`CODEBUDDY_AUTH_TOKEN` 换成反代
 * 专用密钥；账号由反代服务的账号池在请求粒度调度，所以之后再换号不需要改配置、
 * 也不需要重启 CLI。关闭时按平台恢复直连认证（Windows 写回账号 token，其余交回 helper）。
 */
export function CliLinkCard() {
  const [status, setStatus] = useState<CodeBuddyCliStatus | null>(null);
  const [key, setKey] = useState("");
  const [reveal, setReveal] = useState(false);
  const [baseDraft, setBaseDraft] = useState("");
  const [busy, setBusy] = useState<"" | "load" | "enable" | "disable" | "rotate">("load");
  const [error, setError] = useState("");

  const proxy = status?.proxy;
  const enabled = proxy?.enabled === true;

  const refresh = useCallback(async (quiet = true) => {
    if (!quiet) setBusy("load");
    try {
      const next = await getCodebuddyCliStatus();
      try {
        const svc = await proxyServiceStatus();
        if (svc.baseUrl && !baseDraft) setBaseDraft(`${svc.baseUrl}/v1`);
      } catch { /* 服务状态拉不到时保留手填 */ }
      setStatus(next);
      if (!quiet) setError("");
    } catch (cause) {
      if (!quiet) setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (!quiet) setBusy("");
    }
  }, []);

  useEffect(() => {
    void refresh(false);
  }, [refresh]);

  /** 取回当前 CLI 密钥（幂等：服务端没有才新建）。 */
  async function ensureKey(): Promise<string> {
    if (key) return key;
    const result = await rp.cliKey();
    setKey(result.key);
    return result.key;
  }

  async function enable() {
    setBusy("enable");
    setError("");
    try {
      const token = await ensureKey();
      // 用户可能只填了服务地址：归一化逻辑见 lib/cli-link.ts（有单测）。
      const base = normalizeProxyBase(baseDraft);
      if (!base) throw new Error("反代地址不能为空");
      await enableCodebuddyCliProxy(base, token);
      setBaseDraft(base);
      await refresh(false);
      toast.success("已切换为反代模式，重启一次 CLI 生效");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("开启反代模式失败");
    } finally {
      setBusy("");
    }
  }

  async function disable() {
    setBusy("disable");
    setError("");
    try {
      await disableCodebuddyCliProxy();
      await refresh(false);
      toast.success("已关闭反代模式，重启 CLI 后恢复直连");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("关闭反代模式失败");
    } finally {
      setBusy("");
    }
  }

  async function rotate() {
    setBusy("rotate");
    setError("");
    try {
      const result = await rp.rotateCliKey();
      setKey(result.key);
      if (enabled) {
        // 轮换会让旧密钥立即失效；CLI 里存的还是旧的，所以紧接着写回一次，
        // 否则用户会在下一次对话时突然收到 401。
        await enableCodebuddyCliProxy(proxy?.baseUrl || baseDraft, result.key);
        await refresh(false);
        toast.success("已换新密钥并写回 CLI");
      } else {
        toast.success("已换新密钥");
      }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("换密钥失败");
    } finally {
      setBusy("");
    }
  }

  return (
    <div className="space-y-4">
      <PanelHeader
        title="CodeBuddy CLI 接入"
        desc="让 CLI 走本机反代：凭据换成反代密钥，账号由账号池调度，换号不用再重启 CLI。"
        action={
          <ToneBadge tone={enabled ? "ok" : "muted"}>
            {enabled ? "反代模式" : "直连模式"}
          </ToneBadge>
        }
      />

      {error ? <Banner tone="bad">{error}</Banner> : null}

      <div className="grid gap-3 rounded-xl border border-border/70 bg-card p-4 sm:grid-cols-2">
        <Field
          label="反代地址"
          hint="CLI 会在它后面拼 /chat/completions；通常就是本页服务地址加 /v1。"
        >
          <Input
            value={baseDraft}
            onChange={(event) => setBaseDraft(event.target.value)}
            disabled={enabled || busy !== ""}
            placeholder="http://127.0.0.1:8787/v1"
            spellCheck={false}
          />
        </Field>

        <Field
          label="CLI 专用密钥"
          hint={enabled
            ? `当前生效：${proxy?.tokenHint ?? "—"}（换新密钥后需重新开启一次）`
            : "开启时自动签发；与普通 API 密钥分开，可单独吊销。"}
        >
          <div className="flex flex-wrap items-center gap-2">
            <Input
              className="min-w-0 flex-1 font-mono text-xs"
              readOnly
              type={reveal ? "text" : "password"}
              value={key || (enabled ? proxy?.tokenHint ?? "" : "")}
              placeholder="点击「开启反代模式」自动获取"
            />
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => setReveal((value) => !value)}
              disabled={!key}
            >
              {reveal ? "隐藏" : "显示"}
            </Button>
            {key ? <CopyButton value={key} label="复制" /> : null}
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={() => void rotate()}
              disabled={busy !== ""}
            >
              {busy === "rotate" ? <Loader2 className="size-3.5 animate-spin" /> : <RotateCw className="size-3.5" />}
              换一枚
            </Button>
          </div>
        </Field>
      </div>

      <div className="flex flex-wrap items-center gap-2">
        {enabled ? (
          <Button variant="outline" onClick={() => void disable()} disabled={busy !== ""}>
            {busy === "disable" ? <Loader2 className="size-4 animate-spin" /> : <Unplug className="size-4" />}
            关闭反代模式
          </Button>
        ) : (
          <Button onClick={() => void enable()} disabled={busy !== "" || !baseDraft.trim()}>
            {busy === "enable" ? <Loader2 className="size-4 animate-spin" /> : <Terminal className="size-4" />}
            开启反代模式
          </Button>
        )}
        <Button variant="ghost" size="sm" onClick={() => void refresh(false)} disabled={busy !== ""}>
          <RefreshCw className="size-3.5" />
          刷新状态
        </Button>
        {status ? (
          <span className="text-[11px] text-muted-foreground">
            账号 {status.accountCount} 个 · 当前认证 {status.authMode ?? "—"}
          </span>
        ) : null}
      </div>

      <Banner tone="muted">
        开启/关闭都需要<strong>重启一次 CodeBuddy CLI</strong> 让 settings.json 生效；之后在「账号池」换号
        即时生效，不需要再重启。反代模式只写 ~/.codebuddy/settings.json，不改账号库。
      </Banner>
    </div>
  );
}