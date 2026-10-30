import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { rp, apiBaseFromService, type RpAccount, type RpOverview } from "@/lib/reverse-proxy";
import { Banner, CodeBlock, CopyButton, Field, PanelHeader, ToneBadge, fmtTime } from "./rp-shared";

/** 服务端没给默认模型（旧版本）时退回的字面量，与服务端 converter.DEFAULT_MODEL 保持一致。 */
const FALLBACK_MODEL = "deepseek-v4.1-flash";

/**
 * 测试表单默认选中哪个模型。
 *
 * 优先用服务端自报的默认模型（客户端不带 model 时它替我们选的那个），
 * 这样「控制台默认值」和「真实请求默认值」不会是两回事；服务端没报或不在清单里，
 * 再退回清单首项、最后退回字面量。
 */
function pickDefaultModel(models: string[], serviceDefault?: string): string {
  if (serviceDefault && models.includes(serviceDefault)) return serviceDefault;
  return models[0] ?? FALLBACK_MODEL;
}

export function TestPanel({ overview, active }: { overview: RpOverview; active?: RpAccount }) {
  const models = overview.models;
  const preferred = pickDefaultModel(models, overview.default_model);
  const [model, setModel] = useState(preferred);
  const [prompt, setPrompt] = useState("请只回复：连接成功");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; status?: number; answer?: string; error?: string; seconds: number; usage?: { total_tokens?: number } } | null>(null);

  // 模型清单是异步到的，首帧往往还是空的。清单到齐后补上默认模型；
  // 用户已经手动选过（选中的在清单里）就不动他。
  useEffect(() => {
    setModel((prev) => (prev && models.includes(prev) ? prev : preferred));
  }, [preferred, models]);

  async function send() {
    setBusy(true);
    setResult(null);
    try {
      const r = await rp.testModel(model, prompt);
      setResult(r);
    } catch (cause) {
      setResult({ ok: false, error: cause instanceof Error ? cause.message : String(cause), seconds: 0 });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PanelHeader title="连接测试" desc="使用当前选中的账号发起一条真实请求，会消耗少量上游额度。" />

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="flex flex-col gap-3 rounded-xl border border-border/70 bg-card p-4">
          <h3 className="text-sm font-medium">发送一条真实请求</h3>
          <div className="rounded-lg bg-muted/50 px-3 py-2 text-xs">
            当前账号 <b>{active?.name ?? "尚未选择（将按调度模式分配）"}</b>
          </div>
          <Field label="测试模型">
            <Select value={model} onValueChange={setModel}>
              <SelectTrigger className="h-9 text-sm"><SelectValue placeholder="选择模型" /></SelectTrigger>
              <SelectContent>
                {models.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
              </SelectContent>
            </Select>
          </Field>
          <Field label="消息">
            <textarea
              className="min-h-28 w-full rounded-md border border-input bg-transparent px-3 py-2 text-sm outline-none focus-visible:border-ring"
              maxLength={2000}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
            />
          </Field>
          <Button size="sm" className="self-start" disabled={busy || !prompt.trim() || !model} onClick={() => void send()}>
            {busy ? "正在调用…" : "发送测试"}
          </Button>
        </section>

        <section className="flex flex-col rounded-xl border border-border/70 bg-card p-4">
          <div className="flex items-center justify-between">
            <h3 className="text-sm font-medium">响应结果</h3>
            <ToneBadge tone={busy ? "warn" : result ? (result.ok ? "ok" : "bad") : "muted"}>
              {busy ? "请求中" : result ? (result.ok ? "连接成功" : "调用失败") : "等待请求"}
            </ToneBadge>
          </div>
          <pre className="mt-3 min-h-40 flex-1 overflow-auto whitespace-pre-wrap break-words rounded-lg bg-muted/40 p-3 text-xs leading-5">
            {busy ? "正在等待上游响应，最长约 90 秒…" : result ? (result.ok ? result.answer : result.error) : "选择模型并发送消息，结果将在这里显示。"}
          </pre>
          {result ? (
            <div className="mt-2 font-mono text-[11px] text-muted-foreground">
              {result.seconds}s{result.status ? ` · HTTP ${result.status}` : ""}{result.usage?.total_tokens !== undefined ? ` · ${result.usage.total_tokens} tokens` : ""}
            </div>
          ) : null}
        </section>
      </div>

      <section className="rounded-xl border border-border/70 bg-card p-4">
        <h3 className="text-sm font-medium">本次运行的测试记录</h3>
        <p className="mt-0.5 text-xs text-muted-foreground">仅记录状态与耗时，不记录测试消息或回复。</p>
        <div className="mt-3 flex flex-col gap-1.5">
          {overview.events.map((e, i) => (
            <div key={i} className="flex items-center gap-3 text-xs">
              <span className="font-mono text-[10px] text-muted-foreground">{fmtTime(e.time)}</span>
              <span className="font-medium">{e.model}</span>
              <ToneBadge tone={e.ok ? "ok" : "bad"}>{e.ok ? "成功" : "失败"}</ToneBadge>
              <span className="font-mono">{e.seconds}s</span>
            </div>
          ))}
          {overview.events.length === 0 ? <p className="text-xs text-muted-foreground">暂无测试记录，发送第一条测试消息。</p> : null}
        </div>
      </section>
    </div>
  );
}

const ENDPOINTS: [string, string, string][] = [
  ["POST", "/v1/chat/completions", "OpenAI Chat Completions"],
  ["POST", "/v1/responses", "OpenAI Responses（Codex CLI）"],
  ["POST", "/v1/messages", "Anthropic Messages（Claude Code / cc-switch）"],
  ["POST", "/v1/messages/count_tokens", "输入 token 粗估"],
  ["GET", "/v1/models", "内置模型列表"],
];

export function GuidePanel({ base: baseProp }: { base?: string } = {}) {
  const base = apiBaseFromService(baseProp);
  const credentialDir = "%LOCALAPPDATA%\\CodeBuddyExtension\\Data\\Public\\auth";
  return (
    <div className="flex flex-col gap-5">
      <PanelHeader title="接入指南" desc="从导入凭据到客户端接入，只需几步。" />

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="flex flex-col gap-3 rounded-xl border border-border/70 bg-card p-4">
          <span className="text-[11px] font-medium tracking-wide text-muted-foreground">01 / 凭据</span>
          <h3 className="text-sm font-medium">添加登录凭据</h3>
          <p className="text-xs leading-5 text-muted-foreground">
            优先使用「账号池 → 添加账号 → 浏览器登录」，在官方页面授权后自动保存，无需桌面客户端。
            也可在桌面端登录后，按 Win + R 打开以下目录导入文件：
          </p>
          <CodeBlock action={<CopyButton value={credentialDir} />}>{credentialDir}</CodeBlock>
          <p className="text-xs leading-5 text-muted-foreground">
            将其中的 <code className="font-mono">.info</code> 文件拖入「添加账号」，或粘贴其 JSON 内容。
            浏览器授权与文件导入可同时使用。
          </p>
        </section>

        <section className="flex flex-col gap-3 rounded-xl border border-border/70 bg-card p-4">
          <span className="text-[11px] font-medium tracking-wide text-muted-foreground">02 / 客户端</span>
          <h3 className="text-sm font-medium">连接你的客户端</h3>
          <Field label="OpenAI / Responses Base URL"><CodeBlock action={<CopyButton value={base + "/v1"} />}>{base + "/v1"}</CodeBlock></Field>
          <Field label="Anthropic Base URL"><CodeBlock action={<CopyButton value={base} />}>{base}</CodeBlock></Field>
          <p className="text-xs leading-5 text-muted-foreground">
            在「API 密钥」创建 Key 后填入客户端；模型可先用 <code className="font-mono">deepseek-v4.1-flash</code>，
            其它模型是否可用取决于账号权限与密钥白名单。
          </p>
        </section>
      </div>

      <section className="rounded-xl border border-border/70 bg-card p-4">
        <h3 className="text-sm font-medium">支持的接口</h3>
        <div className="mt-3 flex flex-col gap-2">
          {ENDPOINTS.map(([method, path, desc]) => (
            <div key={path} className="flex flex-wrap items-center gap-3 text-xs">
              <ToneBadge tone={method === "GET" ? "muted" : "ok"}>{method}</ToneBadge>
              <code className="font-mono">{path}</code>
              <span className="text-muted-foreground">{desc}</span>
              <CopyButton className="ml-auto h-7" value={base + path} label="复制" />
            </div>
          ))}
        </div>
      </section>

      <Banner>
        密钥可分别限定来源 IP、可用模型与用量额度；管理后台默认仅监听回环地址，如需公网访问请自行加反代并配合「IP 安全」白名单。
      </Banner>
    </div>
  );
}
