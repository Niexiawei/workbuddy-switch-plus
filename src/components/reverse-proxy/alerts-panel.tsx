import { useCallback, useEffect, useState } from "react";
import { Loader2, Save } from "lucide-react";
import { toast } from "sonner";

import { Banner, PanelHeader } from "@/components/reverse-proxy/rp-shared";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  ALERT_PASSWORD_MASK,
  pickAlertPatch,
  rp,
  type RpAlertFlag,
  type RpAlerts,
} from "@/lib/reverse-proxy";

/**
 * 告警通知。
 *
 * 与网页端管理后台共用同一份配置：改任意一端都立即对另一端生效。
 *
 * ⚠️ PUT 是**浅合并**：只发改动的键，绝不整份回传 —— 否则两端各自拼的对象
 * 会互相覆盖，表现成「网页端刚改完，桌面端那边没了」。
 *
 * ⚠️ **口令字段有陷阱**：服务端只回掩码，所以载入后 `password` 是
 * `********` 而不是真口令。保存时必须用 `pickAlertPatch` 把它剔掉，
 * 否则会把真口令覆盖成这串星号 —— 界面还显示保存成功，
 * 但之后所有告警邮件都发不出去。这是本面板最容易踩的坑。
 */

/** 10 个开关，按语义分组。顺序与后端 alerts.FLAG_KEYS 一致。 */
const FLAG_GROUPS: { label: string; flags: { key: RpAlertFlag; label: string; hint?: string }[] }[] = [
  {
    label: "账号类",
    flags: [
      { key: "notify_on_banned", label: "账号封禁" },
      { key: "notify_on_frozen", label: "模型冻结" },
      { key: "notify_on_upstream_disabled", label: "上游禁用" },
      { key: "notify_on_refresh_failed", label: "凭据刷新失败" },
    ],
  },
  {
    label: "额度类",
    flags: [
      { key: "notify_on_credits", label: "积分用量告急" },
      { key: "notify_on_quota", label: "密钥额度告急" },
      { key: "notify_on_key_rejected", label: "密钥被拒" },
    ],
  },
  {
    label: "池与数据",
    flags: [
      { key: "notify_on_pool_empty", label: "可用账号池为空" },
      {
        key: "notify_on_pool_thin",
        label: "可用账号偏少",
        // 默认关闭，说清原因 —— 否则用户会以为这是漏配的。
        hint: "默认关闭：池子经常短暂变浅，开着容易被无意义的提醒淹没。",
      },
      { key: "notify_on_stale", label: "数据陈旧" },
    ],
  },
];

export function AlertsPanel() {
  const [cfg, setCfg] = useState<RpAlerts | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  /** 用户是否动过口令框。没动过就不提交这个键。 */
  const [passwordTouched, setPasswordTouched] = useState(false);
  const [error, setError] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    setError("");
    try {
      const next = await rp.alerts();
      setCfg(next);
      setPasswordTouched(false);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { void load(); }, [load]);

  function patch(next: Partial<RpAlerts>) {
    setCfg((current) => (current ? { ...current, ...next } : current));
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setError("");
    try {
      // 只发用户真正改过的字段。口令没动过就整个不发 ——
      // 这是掩码陷阱的第二道保险（第一道在 pickAlertPatch）。
      const body: Partial<RpAlerts> = passwordTouched
        ? cfg
        : { ...cfg, password: ALERT_PASSWORD_MASK };
      const next = await rp.saveAlerts(pickAlertPatch(body));
      setCfg(next);
      setPasswordTouched(false);
      toast.success("告警设置已保存");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }

  if (loading) {
    return (
      <div className="flex flex-col gap-5">
        <PanelHeader title="告警通知" desc="账号出现封禁、冻结、积分告急时发邮件提醒你。" />
        <div className="rounded-xl border border-border/70 px-4 py-10 text-center text-xs text-muted-foreground">
          正在读取…
        </div>
      </div>
    );
  }

  if (!cfg) {
    return (
      <div className="flex flex-col gap-5">
        <PanelHeader title="告警通知" desc="账号出现封禁、冻结、积分告急时发邮件提醒你。" />
        {error ? <Banner tone="bad">{error}</Banner> : <Banner tone="muted">读取告警设置失败。</Banner>}
        <div><Button size="sm" variant="outline" onClick={() => void load()}>重试</Button></div>
      </div>
    );
  }

  const num = (value: number | undefined, fallback: number) =>
    Number.isFinite(value) ? (value as number) : fallback;

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader
        title="告警通知"
        desc="只发给下面填写的收件地址，不涉及任何终端用户。与网页端是同一份配置。"
      />

      {error ? <Banner tone="bad">{error}</Banner> : null}

      {/* ── 开关与收件人 ── */}
      <section className="rounded-xl border border-border/70 bg-card">
        <div className="flex items-center justify-between border-b border-border/60 px-4 py-2.5">
          <span className="text-xs font-medium">发送</span>
          <label className="flex items-center gap-2 text-xs">
            <input
              type="checkbox"
              className="size-3.5 accent-current"
              checked={cfg.enabled}
              onChange={(e) => patch({ enabled: e.target.checked })}
            />
            启用告警邮件
          </label>
        </div>
        <div className="flex flex-col gap-4 px-4 py-4">
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-to">
              收件地址
            </label>
            <Input
              id="al-to"
              className="max-w-md"
              value={cfg.to}
              placeholder="you@example.com"
              maxLength={500}
              onChange={(e) => patch({ to: e.target.value })}
            />
            <p className="mt-1 text-[11px] leading-4 text-muted-foreground">
              多个地址用逗号分隔。
            </p>
          </div>
        </div>
      </section>

      {/* ── SMTP ── */}
      <section className="rounded-xl border border-border/70 bg-card">
        <div className="border-b border-border/60 px-4 py-2.5">
          <span className="text-xs font-medium">SMTP 服务器</span>
        </div>
        <div className="grid gap-4 px-4 py-4 sm:grid-cols-2">
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-host">
              SMTP 服务器
            </label>
            <Input
              id="al-host"
              value={cfg.host}
              placeholder="smtp.example.com"
              onChange={(e) => patch({ host: e.target.value })}
            />
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-port">
              端口
            </label>
            <Input
              id="al-port"
              type="number"
              min={1}
              max={65535}
              value={String(num(cfg.port, 465))}
              onChange={(e) => patch({ port: Number(e.target.value) })}
            />
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-tls">
              加密方式
            </label>
            <select
              id="al-tls"
              className="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
              value={cfg.tls}
              onChange={(e) => patch({ tls: e.target.value })}
            >
              <option value="ssl">SSL（465，推荐）</option>
              <option value="starttls">STARTTLS（587）</option>
              <option value="none">不加密</option>
            </select>
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-user">
              用户名
            </label>
            <Input
              id="al-user"
              value={cfg.username}
              onChange={(e) => patch({ username: e.target.value })}
            />
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-pass">
              口令
            </label>
            <Input
              id="al-pass"
              type="password"
              value={cfg.password}
              placeholder={cfg.password_set ? "已配置（留空则不改）" : "尚未配置"}
              onChange={(e) => { setPasswordTouched(true); patch({ password: e.target.value }); }}
            />
            <p className="mt-1 text-[11px] leading-4 text-muted-foreground">
              {cfg.password_set
                ? "已配置。这里显示的是掩码，不改动就不会覆盖已存的口令。"
                : "尚未配置。"}
            </p>
          </div>
          <div>
            <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor="al-sender">
              发件人
            </label>
            <Input
              id="al-sender"
              value={cfg.sender}
              placeholder="留空则用用户名"
              onChange={(e) => patch({ sender: e.target.value })}
            />
          </div>
        </div>
      </section>

      {/* ── 告警类型 ── */}
      <section className="rounded-xl border border-border/70 bg-card">
        <div className="border-b border-border/60 px-4 py-2.5">
          <span className="text-xs font-medium">告警类型</span>
        </div>
        <div className="flex flex-col gap-4 px-4 py-4">
          {FLAG_GROUPS.map((group) => (
            <div key={group.label}>
              <div className="mb-2 text-[11px] font-medium tracking-wide text-muted-foreground/80">
                {group.label}
              </div>
              <div className="flex flex-wrap gap-x-5 gap-y-2">
                {group.flags.map((flag) => (
                  <label key={flag.key} className="flex items-center gap-2 text-xs">
                    <input
                      type="checkbox"
                      className="size-3.5 accent-current"
                      checked={cfg[flag.key] !== false}
                      onChange={(e) => patch({ [flag.key]: e.target.checked } as Partial<RpAlerts>)}
                    />
                    <span>{flag.label}</span>
                  </label>
                ))}
              </div>
              {group.flags.some((f) => f.hint) ? (
                <p className="mt-1.5 text-[11px] leading-4 text-muted-foreground">
                  {group.flags.filter((f) => f.hint).map((f) => f.hint).join(" ")}
                </p>
              ) : null}
            </div>
          ))}
        </div>
      </section>

      {/* ── 阈值 ── */}
      <section className="rounded-xl border border-border/70 bg-card">
        <div className="border-b border-border/60 px-4 py-2.5">
          <span className="text-xs font-medium">触发阈值</span>
        </div>
        <div className="grid gap-4 px-4 py-4 sm:grid-cols-2 lg:grid-cols-3">
          {([
            ["credits_threshold", "积分告急阈值", "账号剩余积分低于或等于该值时提醒。", 0, 10_000_000],
            ["quota_percent", "额度使用率（%）", "密钥额度用到该比例时提醒。", 1, 100],
            ["pool_min_available", "可用账号下限", "池内可用账号少于该数量时提醒。", 0, 10_000],
            ["stale_minutes", "数据陈旧（分钟）", "超过该时长未更新时提醒。", 1, 10_080],
            ["key_reject_threshold", "密钥被拒次数", "同一密钥在该次数内被拒就提醒。", 1, 10_000],
          ] as const).map(([key, label, hint, min, max]) => (
            <div key={key}>
              <label className="mb-1.5 block text-xs font-medium text-foreground/90" htmlFor={`al-${key}`}>
                {label}
              </label>
              <Input
                id={`al-${key}`}
                type="number"
                min={min}
                max={max}
                value={String(num(cfg[key] as number | undefined, min))}
                onChange={(e) => patch({ [key]: Number(e.target.value) } as Partial<RpAlerts>)}
              />
              <p className="mt-1 text-[11px] leading-4 text-muted-foreground">{hint}</p>
            </div>
          ))}
        </div>
      </section>

      <div className="flex items-center justify-between gap-3">
        {/*
          测试邮件在内置服务（Rust）下仍是 501 —— SMTP 客户端还没迁。
          按「不摆点了会报错的按钮」的约定，这里直接不给按钮，改为指路。
          告警配置本身仍然可保存：只是暂时没法自证发得出去。
        */}
        <span className="text-[11px] leading-4 text-muted-foreground">
          测试邮件需要 SMTP 客户端，内置服务暂未支持；请到「反代服务old」里发送。
        </span>
        <Button size="sm" disabled={saving} onClick={() => void save()}>
          {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
          {saving ? "保存中…" : "保存"}
        </Button>
      </div>

      <p className="text-[11px] leading-4 text-muted-foreground">
        配置类功能最可靠的自证方式是发一封测试邮件 —— 否则只能等下一次真告警才知道
        「原来没配通」，而那时候问题已经发生了。
      </p>
    </div>
  );
}
