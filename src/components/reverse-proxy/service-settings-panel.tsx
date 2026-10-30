import { useCallback, useEffect, useState } from "react";
import { Loader2, Save } from "lucide-react";
import { toast } from "sonner";

import { Banner, PanelHeader } from "@/components/reverse-proxy/rp-shared";
import { Button } from "@/components/ui/button";
import { rp, type RpServiceSettings } from "@/lib/reverse-proxy";

/**
 * 服务设置。
 *
 * 与网页端管理后台共用同一份配置：改任意一端都立即对另一端生效。
 *
 * ⚠️ PUT 是**浅合并**（实测确认）：只发「改动的键」，绝不整份回传 ——
 * 整份回传会让网页端与桌面端各自拼的对象互相覆盖，
 * 表现成「网页端刚改完，桌面端那边没了」。
 */
export function ServiceSettingsPanel() {
  const [settings, setSettings] = useState<RpServiceSettings | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      setSettings(await rp.settings());
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  async function save() {
    if (!settings) return;
    setSaving(true);
    setError("");
    try {
      // 没有可编辑字段（site_name 已退役）；保留保存按钮位是为了 old 页操作习惯不变。
      setSettings(await rp.saveSettings({}));
      toast.success("服务设置已保存");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }

  if (loading) {
    return (
      <div className="flex flex-col gap-5">
        <PanelHeader title="服务设置" desc="与网页端共用同一份配置。" />
        <div className="rounded-xl border border-border/70 px-4 py-10 text-center text-xs text-muted-foreground">
          正在读取…
        </div>
      </div>
    );
  }

  if (!settings) {
    return (
      <div className="flex flex-col gap-5">
        <PanelHeader title="服务设置" desc="与网页端共用同一份配置。" />
        {error ? <Banner tone="bad">{error}</Banner> : <Banner tone="muted">读取设置失败。</Banner>}
        <div><Button size="sm" variant="outline" onClick={() => void load()}>重试</Button></div>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="服务设置"
        desc="这里与网页端管理后台是同一份配置，改任意一端都立即对另一端生效。"
      />

      {error ? <Banner tone="bad">{error}</Banner> : null}

      <section className="rounded-xl border border-border/70 bg-card">
        <div className="border-b border-border/60 px-4 py-2.5">
          <span className="text-xs font-medium">基础</span>
        </div>
        <div className="flex flex-col gap-4 px-4 py-4">
          {settings.settings_key_source ? (
            <p className="text-[11px] leading-4 text-muted-foreground">
              配置密钥来源：<code className="font-mono">{settings.settings_key_source}</code>
            </p>
          ) : null}
        </div>
      </section>

      <div className="flex justify-end">
        <Button size="sm" disabled={saving} onClick={() => void save()}>
          {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
          {saving ? "保存中…" : "保存"}
        </Button>
      </div>
    </div>
  );
}
