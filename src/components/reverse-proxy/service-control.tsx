import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { AlertTriangle, ChevronDown, ChevronRight, FolderOpen, FolderSearch, Globe, Hash, Play, RefreshCw, ShieldAlert, Square, Terminal } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Banner, CodeBlock, CopyButton, ToneBadge } from "@/components/reverse-proxy/rp-shared";
import {
  adoptBaseUrl, serviceOpenDataDir, serviceRestartWithHost, serviceRestartWithPort, serviceSetExe,
  serviceStart, serviceStatus, serviceStop, serviceStopElevated,
  serviceSupported,
  type RpServiceStatus,
} from "@/lib/reverse-proxy-service";

/**
 * 反代服务的进程控制台。
 *
 * 桌面端可以自己把反代服务拉起来，所以这里给一个明确的「启动」入口——用户不该
 * 为了进管理页先去开终端敲命令。浏览器形态下没有进程可管，退化成一条说明。
 */
export function ServiceControl({ onServiceUp }: { onServiceUp?: () => void }) {
  const supported = serviceSupported();
  // 引擎切换已退场：这一屏只服务「反代服务old」（Python 独立进程）。
  // 内置 Rust 反代由 uiltin-service-card.tsx 管理，不再走这里。
  const rustEngine = false;
// old 的默认服务地址（LEGACY_PORT=8788）：地址展示在未运行时用它。
const LEGACY_BASE = "http://127.0.0.1:8788";
  const [status, setStatus] = useState<RpServiceStatus | null>(null);
  const [busy, setBusy] = useState<"" | "start" | "stop" | "elevate" | "exe" | "open" | "refresh" | "lan" | "port">("");
  const [error, setError] = useState("");
  const [showLog, setShowLog] = useState(false);
  /** 换端口输入框。与 status.port 分开存：输入过程中允许是半截数字。 */
  const [portDraft, setPortDraft] = useState("");
  // 普通停止失败过之后，把「以管理员身份停止」顶到显眼位置——大多数停不掉都是权限问题。
  const [stopFailed, setStopFailed] = useState(false);
  const upRef = useRef(false);

  const apply = useCallback((next: RpServiceStatus) => {
    setStatus(next);
    // 服务刚起来时通知外层重试一次会话（可能已经有令牌了）。
    if (next.running && !upRef.current) {
      upRef.current = true;
      onServiceUp?.();
    }
    if (!next.running) upRef.current = false;
  }, [onServiceUp]);

  const refresh = useCallback(async (quiet = true) => {
    if (!supported) return;
    if (!quiet) setBusy("refresh");
    try {
      apply(await serviceStatus());
      if (!quiet) setError("");
    } catch (cause) {
      if (!quiet) setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (!quiet) setBusy("");
    }
  }, [apply, supported]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // 轮询进程/端口状态：本机调用很轻，5 秒足够跟上「启动中 → 就绪」的变化。
  useEffect(() => {
    if (!supported) return;
    const id = window.setInterval(() => {
      if (document.hidden) return;
      void refresh();
    }, 5000);
    return () => window.clearInterval(id);
  }, [refresh, supported]);

  async function start() {
    setBusy("start");
    setError("");
    try {
      const next = await serviceStart(status?.port);
      adoptBaseUrl(next);
      apply(next);
      toast.success("反代服务已启动");
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
      apply(await serviceStop());
      setStopFailed(false);
      toast.success("反代服务已停止");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      setStopFailed(true);
      toast.error("停止失败");
    } finally {
      setBusy("");
    }
  }

  async function stopElevated() {
    if (rustEngine) {
      toast.error("Rust 引擎由应用托管，不需要提权停止");
      return;
    }
    setBusy("elevate");
    setError("");
    try {
      const message = await serviceStopElevated();
      toast.success(message);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("提权停止失败");
    } finally {
      setBusy("");
    }
  }

  /**
   * 以 0.0.0.0 重启服务，让局域网内的其他设备也能访问。
   *
   * 监听地址是进程启动参数，改不了只能重启，所以这里一定是「先停后启」。
   * 重启会短暂中断正在转发的请求 —— 提前用 toast 说清楚，别让用户以为服务崩了。
   */
  async function restartOnLan() {
    if (rustEngine) {
      toast.error("Rust 引擎由应用托管，不支持改监听地址；需要时用 --port 启动");
      return;
    }
    if (!window.confirm(
      "将以 0.0.0.0 重启反代服务，本机所在网络内的其他设备都能访问。\n\n" +
      "· 服务会短暂中断几秒，正在转发的请求会失败重试。\n" +
      "· 请确认这台机器的网络是可信的。\n\n" +
      "继续吗？",
    )) return;
    setBusy("lan");
    setError("");
    try {
      const next = await serviceRestartWithHost("0.0.0.0");
      adoptBaseUrl(next);
      apply(next);
      toast.success("已以 0.0.0.0 重启，局域网内可访问");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("重启失败");
    } finally {
      setBusy("");
    }
  }

  /** 改回只绑本机。同样要重启。 */
  async function restartOnLocalhost() {
    if (rustEngine) {
      toast.error("Rust 引擎由应用托管，不支持改监听地址");
      return;
    }
    setBusy("lan");
    setError("");
    try {
      const next = await serviceRestartWithHost("127.0.0.1");
      adoptBaseUrl(next);
      apply(next);
      toast.success("已改回只绑本机");
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("重启失败");
    } finally {
      setBusy("");
    }
  }

  /**
   * 换端口重启。
   *
   * 三条前置校验都在发请求之前做，理由相同：换个端口失败的结果是**旧服务已经停了、
   * 新服务起不来**，用户从「能用」掉到「完全不能用」。能提前拦的一律提前拦。
   *
   * 端口下限 1024：低于它的端口在 Windows 上通常需要管理员权限，
   * 而且 1–1023 里有一堆系统服务，撞上一个就是起不来。
   */
  async function restartOnPort() {
    if (rustEngine) {
      toast.error("Rust 引擎端口由应用管理，暂不支持页面内改端口");
      return;
    }
    const next = Number(portDraft.trim());
    if (!Number.isFinite(next) || !Number.isInteger(next)) {
      setError("端口必须是整数");
      return;
    }
    if (next < 1024 || next > 65535) {
      setError("端口需在 1024–65535 之间");
      return;
    }
    if (running && next === status?.port) {
      setError("新端口和当前端口一样，没有需要重启的");
      return;
    }
    if (!window.confirm(
      `将把反代服务重启到端口 ${next}。\n\n` +
      "· 服务会中断几秒，正在转发的请求会失败重试。\n" +
      "· 客户端里配置的地址要跟着改成新端口，否则会连不上。\n\n" +
      "继续吗？",
    )) return;
    setBusy("port");
    setError("");
    try {
      const result = await serviceRestartWithPort(next);
      adoptBaseUrl(result);
      apply(result);
      setPortDraft("");
      toast.success(`已重启到端口 ${next}`);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("换端口失败");
    } finally {
      setBusy("");
    }
  }

  async function openDataDir() {
    setBusy("open");
    setError("");
    try {
      await serviceOpenDataDir();
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      toast.error("打开目录失败");
    } finally {
      setBusy("");
    }
  }

  async function pickExe() {
    setBusy("exe");
    setError("");
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({
        multiple: false,
        directory: false,
        title: "选择反代服务可执行文件",
        filters: [{ name: "可执行文件", extensions: ["exe"] }],
      });
      if (typeof picked === "string") apply(await serviceSetExe(picked));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy("");
    }
  }

  async function resetExe() {
    setBusy("exe");
    setError("");
    try {
      apply(await serviceSetExe(null));
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy("");
    }
  }

  if (!supported) {
    return (
      <Banner tone="muted">
        当前是浏览器形态，管理界面连接的是已经独立部署的反代服务，因此这里没有进程开关。
        用桌面端打开本页即可在界面里直接启动 / 停止服务。
      </Banner>
    );
  }

  const running = status?.running ?? false;
  const tone = running ? "ok" : status?.starting ? "warn" : "muted";
  const label = running ? "运行中" : status?.starting ? "启动中" : "已停止";
  // 判断当前是不是对外监听：看 baseUrl 里的主机部分。
  // 用「包含 0.0.0.0」而不是严格相等，是因为 baseUrl 还带着协议与端口
  // （如 http://0.0.0.0:8787），拆出来比对要多一层解析、还容易在
  // IPv6/自定义主机上出岔子；这里只是决定按钮显示哪个文案，宽松一点没有副作用。
  const lanExposed = running && /(^|\/\/)0\.0\.0\.0([:/]|$)/.test(status?.baseUrl ?? "");

  return (
    <section className="min-w-0 rounded-xl border border-border/70 bg-card">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-border/70 px-4 py-3">
        <div className="flex min-w-0 items-center gap-2">
          <Terminal className="size-4 text-primary" />
          <h2 className="text-sm font-medium">反代服务</h2>
          <ToneBadge tone={tone}>{label}</ToneBadge>
          {running && status ? (
            <span className="truncate font-mono text-[11px] text-muted-foreground">
              {status.baseUrl}
              {status.managed && status.pid ? ` · pid ${status.pid}` : " · 由外部启动"}
            </span>
          ) : null}
        </div>
        <div className="flex items-center gap-2">
          <Button variant="ghost" size="sm" disabled={busy !== ""} onClick={() => void refresh(false)}>
            <RefreshCw className={busy === "refresh" ? "size-3.5 animate-spin" : "size-3.5"} />刷新
          </Button>
          {running ? (
            <>
              {/* 停止按钮只在 running 时禁不禁用，不看 `managed`：服务常常不是本次桌面端
                  拉起的（用户自己双击了 exe，或桌面端重启后旧进程还在跑——关闭桌面端不停
                  服务本来就是刻意设计），拿 managed 当禁用条件会让人误以为「点了没效果」。 */}
              <Button variant="outline" size="sm" disabled={busy !== ""} onClick={() => void stop()}>
                <Square className="size-3.5" />{busy === "stop" ? "停止中…" : "停止服务"}
              </Button>
              {/* 监听地址是 uvicorn 的启动参数，唯一改法是重启。
                  runningBaseUrl 里如果已经是 0.0.0.0，这个按钮就变成「改回只绑本机」——
                  否则用户开了之后没有任何界面路径能关掉它。 */}
              <Button
                variant="ghost"
                size="sm"
                disabled={busy !== ""}
                title={lanExposed
                  ? "改回只允许本机访问（会重启服务）"
                  : "以 0.0.0.0 重启，让局域网内其他设备也能访问（会重启服务）"}
                onClick={() => void (lanExposed ? restartOnLocalhost() : restartOnLan())}
              >
                <Globe className="size-3.5" />
                {busy === "lan" ? "重启中…" : lanExposed ? "只绑本机" : "对局域网开放"}
              </Button>
              {/* 普通停止是在进程内直接结束占用端口的进程；只有那个进程属于别的用户
                  或更高权限时才会被拒，这时才需要提权。失败过就把它变成主按钮。 */}
              <Button
                variant={stopFailed ? "default" : "ghost"}
                size="sm"
                disabled={busy !== ""}
                title="以管理员身份结束占用端口的进程（会弹 UAC）"
                onClick={() => void stopElevated()}
              >
                <ShieldAlert className="size-3.5" />
                {busy === "elevate" ? "提权中…" : stopFailed ? "以管理员身份停止" : "提权停止"}
              </Button>
            </>
          ) : (
            <Button size="sm" disabled={busy !== ""} onClick={() => void start()}>
              <Play className="size-3.5" />{busy === "start" ? "启动中…" : "启动服务"}
            </Button>
          )}
        </div>
      </header>

      <div className="flex min-w-0 flex-col gap-3 px-4 py-3">
        {!running ? (
          <p className="text-xs leading-5 text-muted-foreground">
            服务未运行。点「启动服务」由桌面端直接拉起它（默认监听 127.0.0.1:8787）。
            关闭桌面端不会停止已启动的服务，客户端可以继续用。
          </p>
        ) : null}

        {running && status && !status.managed ? (
          <p className="text-xs leading-5 text-muted-foreground">
            这个服务不是本次桌面端拉起的——可能是你自己双击了 exe，或上次关闭桌面端后它一直在跑，
            因此这里看不到它的输出。「停止服务」会按端口找到它并结束掉。
          </p>
        ) : null}

        {lanExposed ? (
          <Banner tone="warn">
            当前绑定了 0.0.0.0，本机所在网络内的其他设备可以访问这个服务。
            若只是本机使用，点「只绑本机」改回 127.0.0.1。
          </Banner>
        ) : null}

        <div className="grid min-w-0 gap-2">
          <span className="text-[11px] font-medium text-muted-foreground">管理地址</span>
          <CodeBlock action={<CopyButton value={running && status ? status.baseUrl : LEGACY_BASE} />}>
            {running && status ? status.baseUrl : LEGACY_BASE}
          </CodeBlock>
        </div>

        {/* 换端口。输入框常驻而不是塞进弹窗：用户改完端口通常要马上把新地址
            复制给客户端配置，两个动作挨在一起才不会来回找。 */}
        <div className="grid min-w-0 gap-2">
          <span className="text-[11px] font-medium text-muted-foreground">启动端口</span>
          <div className="min-w-0 rounded-lg border border-border/70 bg-muted/50 px-3 py-2">
            <div className="flex flex-wrap items-center gap-1.5">
              <code className="font-mono text-[11px]">当前 {status?.port ?? "—"}</code>
              <input
                className="h-7 w-24 rounded-md border border-input bg-background px-2 font-mono text-[11px] outline-none focus-visible:border-ring"
                inputMode="numeric"
                placeholder="新端口"
                value={portDraft}
                disabled={busy !== ""}
                onChange={(e) => setPortDraft(e.target.value.replace(/[^\d]/g, "").slice(0, 5))}
                onKeyDown={(e) => { if (e.key === "Enter") void restartOnPort(); }}
              />
              <Button
                variant="outline"
                size="sm"
                disabled={busy !== "" || !portDraft.trim()}
                onClick={() => void restartOnPort()}
              >
                <Hash className="size-3.5" />{busy === "port" ? "重启中…" : "换端口重启"}
              </Button>
            </div>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              端口是启动参数，改不了只能重启，所以这里是「先停后启」；服务会中断几秒。
              新端口被占用时不会动手，当前服务保持原样。范围 1024–65535。
            </p>
          </div>
        </div>

        {/* 服务程序路径很长，且旁边还有两个按钮：挤在一行里必然叠字/被卡片裁掉。
            这里改成「路径换行 + 按钮单独一行」，路径能完整读出来。 */}
        <div className="grid min-w-0 gap-2">
          <span className="text-[11px] font-medium text-muted-foreground">服务程序</span>
          <div className="min-w-0 rounded-lg border border-border/70 bg-muted/50 px-3 py-2">
            <code className="block break-all font-mono text-[11px] leading-5">
              {status?.exe ?? (status?.exeExists ? "" : "（未找到）")}
            </code>
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              <Button variant="ghost" size="sm" disabled={busy !== ""} onClick={() => void resetExe()}>自动探测</Button>
              <Button variant="outline" size="sm" disabled={busy !== ""} onClick={() => void pickExe()}>
                <FolderSearch className="size-3.5" />选择…
              </Button>
            </div>
          </div>
        </div>

        {/* 密钥储存位置：管理密钥 / API 密钥 / 管理令牌全在 state.json 里，
            用户要备份或手工核对时得先找得到它。 */}
        <div className="grid min-w-0 gap-2">
          <span className="text-[11px] font-medium text-muted-foreground">密钥储存位置</span>
          <div className="min-w-0 rounded-lg border border-border/70 bg-muted/50 px-3 py-2">
            <code className="block break-all font-mono text-[11px] leading-5">
              {status?.dataDir ?? "（先定位到上方的服务程序，才能算出数据目录）"}
            </code>
            <p className="mt-1 text-[11px] leading-5 text-muted-foreground">
              <code className="font-mono">state.json</code> 就存在这里——管理密钥、API 密钥、管理令牌都在其中。
            </p>
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              <Button
                variant="outline"
                size="sm"
                disabled={busy !== "" || !status?.dataDir}
                onClick={() => void openDataDir()}
              >
                <FolderOpen className="size-3.5" />{busy === "open" ? "打开中…" : "打开"}
              </Button>
              <CopyButton value={status?.dataDir ?? ""} />
            </div>
          </div>
        </div>

        {status && !status.exeExists ? (
          <Banner tone="bad">            <span className="flex items-start gap-2">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                没找到反代服务可执行文件。正式安装版会随安装包一起带进来；
                如果你是从源码运行，先在 <code className="font-mono">reverse-proxy</code> 目录执行{" "}
                <code className="font-mono">python build-exe.py</code> 生成它，或者用上面的「选择…」手动指定。
                {status.searched.length ? (
                  <>
                    <br />已尝试：{status.searched.join("　·　")}
                  </>
                ) : null}
              </span>
            </span>
          </Banner>
        ) : null}

        {status?.firstRunPassword ? (
          <Banner tone="warn">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <span>
                首次启动，服务已生成管理员口令（用户名 <code className="font-mono">admin</code>）。
                请现在保存——它只在这里显示一次。
              </span>
              <span className="flex items-center gap-2">
                <code className="rounded bg-background/60 px-2 py-0.5 font-mono text-[11px]">{status.firstRunPassword}</code>
                <CopyButton value={status.firstRunPassword} label="复制口令" />
              </span>
            </div>
          </Banner>
        ) : null}

        {error ? <Banner tone="bad">{error}</Banner> : null}

        {status?.logTail.length ? (
          <div>
            <button
              type="button"
              className="flex items-center gap-1 text-[11px] font-medium text-muted-foreground hover:text-foreground"
              onClick={() => setShowLog((value) => !value)}
            >
              {showLog ? <ChevronDown className="size-3.5" /> : <ChevronRight className="size-3.5" />}
              服务输出（最近 {status.logTail.length} 行）
            </button>
            {showLog ? (
              <pre className="mt-2 max-h-56 overflow-auto rounded-lg border border-border/70 bg-muted/50 p-3 font-mono text-[11px] leading-5 whitespace-pre-wrap break-all">
                {status.logTail.join("\n")}
              </pre>
            ) : null}
          </div>
        ) : null}
      </div>
    </section>
  );
}