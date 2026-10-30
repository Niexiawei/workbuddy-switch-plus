/**
 * 内置反代服务卡片：状态、启停、端口、数据目录。
 *
 * 与旧的 `service-control.tsx`（Python exe 启动器）是两回事——那份留给
 * 「反代服务old」。这里管的是**进程内**的 awx 监听，没有 exe、没有 pid、
 * 没有提权停止，也没有「关闭应用后继续服务」这种选项。
 *
 * 端口的两个数要同时露出来：`desiredPort` 是用户填的，`port` 是实际在听的。
 * 端口被占时会自动 +1（见 Rust `ports.rs`），只显示一个数会让「我明明设了
 * 8787 为什么客户端连不上」变成一个查不出来的问题。
 */

import { useCallback, useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { FolderOpen, Loader2, Pause, Play, RefreshCw, Save, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Banner, CopyButton, Field, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import { revealErrorLog } from "@/lib/api";
import {
  parsePort,
  portLine,
  proxyServiceSetBind,
  proxyServiceSetPort,
  proxyServiceStart,
  proxyServiceStatus,
  proxyServiceStop,
  type ProxyServiceStatus,
} from "@/lib/proxy-service";

export function BuiltinServiceCard() {
  const navigate = useNavigate();
  const [status, setStatus] = useState<ProxyServiceStatus | null>(null);
  const [portDraft, setPortDraft] = useState("");
  const [busy, setBusy] = useState<"" | "start" | "stop" | "port" | "refresh">("");
  const [error, setError] = useState("");

  const apply = useCallback((next: ProxyServiceStatus) => {
    setStatus(next);
    setPortDraft(String(next.desiredPort));
  }, []);

  const refresh = useCallback(async (quiet = true) => {
    if (!quiet) setBusy("refresh");
    try {
      apply(await proxyServiceStatus());
      if (!quiet) setError("");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (!quiet) setBusy("");
    }
  }, [apply]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 5 秒轮询足够跟上「启动中 → 就绪」与端口被抢占的变化；本机 IPC 很轻。
  useEffect(() => {
    const id = window.setInterval(() => {
      if (document.hidden) return;
      void refresh();
    }, 5000);
    return () => window.clearInterval(id);
  }, [refresh]);

  async function start() {
    setBusy("start");
    setError("");
    try {
      apply(await proxyServiceStart());
      toast.success("内置服务已启动");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("启动失败");
    } finally {
      setBusy("");
    }
  }

  async function stop() {
    setBusy("stop");
    setError("");
    try {
      apply(await proxyServiceStop());
      toast.success("内置服务已停止");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("停止失败");
    } finally {
      setBusy("");
    }
  }

  async function savePort() {
    const port = parsePort(portDraft);
    if (port === null) {
      setError("端口必须在 1–65535 之间");
      return;
    }
    setBusy("port");
    setError("");
    try {
      const next = await proxyServiceSetPort(port);
      apply(next);
      // 端口改了但被占，实际落在下一个可用端口：这是正常的（+1 策略），
      // 但必须说清楚，否则用户会以为设置没生效。
      if (next.portShifted) {
        toast.warning(next.portNotice ?? "端口被占用，已自动顺延");
      } else {
        toast.success(`已切换到 ${port}`);
      }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("改端口失败");
    } finally {
      setBusy("");
    }
  }

  async function saveBind(host: "127.0.0.1" | "0.0.0.0") {
    // 开关切换 = set_bind(host, desiredPort)：一次重绑，端口沿用当前期望值。
    const port = parsePort(portDraft) ?? status?.desiredPort;
    if (!port) {
      setError("端口必须在 1–65535 之间");
      return;
    }
    setBusy("port");
    setError("");
    try {
      const next = await proxyServiceSetBind(host, port);
      apply(next);
      if (host === "0.0.0.0") {
        toast.warning("已对局域网开放。请确认 API 密钥配置了 IP 白名单，否则全网段可用。");
      } else {
        toast.success("已改回只绑本机");
      }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("改监听范围失败");
    } finally {
      setBusy("");
    }
  }

  async function openDataDir() {
    // 数据目录与错误日志同属「本机路径」，沿用主项目已有的 reveal 能力；
    // 内置服务没有独立的「打开目录」命令，不为此新开一个 Tauri 命令。
    try {
      await revealErrorLog();
    } catch {
      setError("无法打开数据目录，请手动前往 ~/.wb-switch-plus/reverse-proxy");
    }
  }

  const running = Boolean(status?.running);
  const draftPort = parsePort(portDraft);
  const portDirty = draftPort !== null && draftPort !== status?.desiredPort;

  return (
    <Card className="min-w-0 gap-0 py-0 shadow-none">
      <CardHeader className="border-b border-border/50 px-5 py-3.5">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle className="text-sm font-medium">内置服务</CardTitle>
          <div className="flex items-center gap-2">
            {running ? <ToneBadge tone="ok">运行中</ToneBadge> : null}
            {status?.phase === "failed" ? <ToneBadge tone="bad">启动失败</ToneBadge> : null}
            {!running && status?.phase !== "failed" ? <ToneBadge tone="muted">已停止</ToneBadge> : null}
            <Button variant="ghost" size="sm" className="h-7 px-2" onClick={() => void refresh(false)} disabled={busy === "refresh"}>
              {busy === "refresh" ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            </Button>
          </div>
        </div>
      </CardHeader>

      <CardContent className="space-y-4 px-5 py-4">
        <p className="text-xs leading-5 text-muted-foreground">
          反代随主程序启动，关闭应用即停止。它是进程内服务，没有独立进程，也没有 exe。
        </p>

        <div className="flex flex-wrap items-center gap-2">
          {running ? (
            <Button size="sm" variant="outline" onClick={() => void stop()} disabled={Boolean(busy)}>
              <Pause className="size-3.5" />停止
            </Button>
          ) : (
            <Button size="sm" onClick={() => void start()} disabled={Boolean(busy)}>
              <Play className="size-3.5" />启动
            </Button>
          )}
        </div>

        {status?.portShifted ? (
          <Banner tone="warn">
            <span className="flex items-start gap-1.5">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0" />
              <span>
                {portLine(status)}。客户端如果写死了 {status.desiredPort}，需要改成实际端口，
                或在下面换一个空闲端口后重新启动。
              </span>
            </span>
          </Banner>
        ) : null}

        {error ? <Banner tone="bad">{error}</Banner> : null}

        <div className="grid gap-4 sm:grid-cols-2">
          <Field
            label="服务地址"
            hint={running ? "外部客户端（Codex / Claude Code）用这个地址" : "服务未运行"}
          >
            <div className="flex items-center gap-1">
              <code className="min-w-0 flex-1 truncate rounded-md border border-border/70 bg-muted/40 px-2 py-1.5 font-mono text-xs">
                {status?.baseUrl ?? "—"}
              </code>
              {status?.baseUrl ? <CopyButton value={status.baseUrl} label="" className="h-7 px-2" /> : null}
            </div>
          </Field>

          <Field label="端口" hint={`默认 8787；被占用时自动顺延，最多试 20 个。${portDirty ? "（未保存）" : ""}`}>
            <div className="flex items-center gap-1.5">
              <Input
                value={portDraft}
                onChange={(event) => setPortDraft(event.target.value)}
                inputMode="numeric"
                placeholder="8787"
                className="h-8 w-24 font-mono text-xs"
              />
              <Button
                size="sm"
                variant="outline"
                className="h-8"
                disabled={busy === "port" || draftPort === null || !portDirty}
                onClick={() => void savePort()}
              >
                {busy === "port" ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
                保存
              </Button>
              {status?.desiredPort && status.desiredPort !== draftPort ? (
                <Button size="sm" variant="ghost" className="h-8" onClick={() => setPortDraft(String(status.desiredPort))}>
                  重置
                </Button>
              ) : null}
            </div>
          </Field>
        </div>

        {/* 监听范围：本机 / 局域网。写进配置（settings.reverse_proxy.host），
            与 Python 形态「每次启动重新确认」不同——内置服务随主程序启动，
            没有重启问答窗口，必须持久化（附录 D）。 */}
        <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-border/60 px-3 py-2.5">
          <div className="min-w-0">
            <p className="text-xs font-medium">对局域网开放</p>
            <p className="mt-0.5 text-[11px] leading-4 text-muted-foreground">
              {status?.lanExposed
                ? "当前绑定 0.0.0.0，局域网内设备可用「本机内网 IP + 端口」访问数据面。"
                : "默认只绑本机（127.0.0.1）。开放后局域网内设备也能调用智能体 API。"}
            </p>
          </div>
          <Button
            size="sm"
            variant={status?.lanExposed ? "outline" : "default"}
            disabled={Boolean(busy)}
            onClick={() => void saveBind(status?.lanExposed ? "127.0.0.1" : "0.0.0.0")}
          >
            {busy === "port" ? <Loader2 className="size-3.5 animate-spin" /> : null}
            {status?.lanExposed ? "只绑本机" : "对局域网开放"}
          </Button>
        </div>

        {status?.lanExposed ? (
          <Banner tone="warn">
            <span className="flex items-start gap-1.5">
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0" />
              <span>
                局域网开放时，API 密钥的 IP 白名单是唯一的访问闸门。
                <Button variant="link" size="sm" className="h-auto px-1 text-[11px]" onClick={() => navigate("/reverse-proxy/keys")}>
                  去「API 密钥」配置白名单
                </Button>
                白名单为空的密钥对全网段可用。
              </span>
            </span>
          </Banner>
        ) : null}

        <div className="flex flex-wrap items-center gap-x-5 gap-y-1 text-[11px] text-muted-foreground">
          <span>
            期望端口 <code className="font-mono">{status?.desiredPort ?? "—"}</code>
          </span>
          <span>
            实际端口 <code className="font-mono">{status?.port ?? "—"}</code>
          </span>
          <Button variant="ghost" size="sm" className="h-6 px-1.5 text-[11px]" onClick={() => void openDataDir()}>
            <FolderOpen className="size-3" />数据目录
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
