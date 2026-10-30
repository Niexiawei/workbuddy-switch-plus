//! 反代服务（wb-reverse-proxy）的进程生命周期管理。
//!
//! 反代服务本身是独立可执行文件：它面向 WorkBuddy / CodeBuddy 客户端提供 API，
//! 生命周期不该绑死在桌面端上——关掉桌面端，已经跑起来的服务仍要继续服务客户端。
//! 所以桌面端在这里只做四件事：**找到它、拉起来、停掉、看状态**，不做常驻托管。
//!
//! 前端只调这里暴露的命令（见 `src/lib/reverse-proxy-service.ts`）。

use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use wb_switch_core::modules::config;

/// 服务默认端口。与 `reverse-proxy/serve.py` 的 `--port` 默认值保持一致。
const DEFAULT_PORT: u16 = 8787;
/// 日志环形缓冲保留的行数：够定位启动失败，又不至于把状态接口撑大。
const LOG_LIMIT: usize = 300;
/// 结构化横幅前缀。改这里要同步改 `reverse-proxy/serve.py`。
const BANNER_PREFIX: &str = "WB_BANNER ";
/// 端口探测超时。本地回环连接很快，300ms 足够且不会卡住状态接口。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// Python 反代服务old 是否在本平台可用。
///
/// Python 版只随 Windows 安装包分发（见 `.github/workflows/build.yml`：
/// 四平台矩阵里只有 win-x64 跑 PyInstaller）。其它平台既没有随包的 exe，
/// 进程管控里的「按端口反查监听者」也没有非 Windows 实现——返回 false，
/// 让前端把 old 的入口整体藏起来，而不是给一个点了必报错的按钮。
pub fn legacy_service_supported() -> bool {
    cfg!(windows)
}

#[derive(Default)]
pub struct ReverseProxy {
    inner: Mutex<Inner>,
}

struct Inner {
    child: Option<Child>,
    pid: Option<u32>,
    port: u16,
    /// 当前这次启动用的监听地址（None = 没传 --host，走服务自己的默认值 127.0.0.1）。
    ///
    /// 这个字段存在只为了一件事：换端口重启时**沿用**原来的监听姿态。
    /// 不记的话只能在 baseUrl 里猜，而 baseUrl 是按端口拼出来的，
    /// `host` 这条信息在拼装时就已经丢了。
    host: Option<String>,
    exe: Option<PathBuf>,
    /// 与读取线程共享：读线程不该再去抢 `inner` 的锁（调用方可能正持锁）。
    log: Arc<Mutex<Vec<String>>>,
    /// 首次启动由服务端生成的管理员口令，从结构化横幅里抓。
    password: Arc<Mutex<Option<String>>>,
    /// Tauri 的资源目录。打包后反代 exe 随安装包一起分发，落在这里；
    /// 开发态由 tauri 拷到 target 目录，同一套逻辑都能命中。
    resource_dir: Option<PathBuf>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            child: None,
            pid: None,
            port: DEFAULT_PORT,
            host: None,
            exe: None,
            log: Arc::new(Mutex::new(Vec::new())),
            password: Arc::new(Mutex::new(None)),
            resource_dir: None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// 端口是否在监听——这是「能否连上」的唯一真相，不依赖是不是本应用拉起的。
    running: bool,
    /// 由本应用拉起，且进程仍然存活。
    managed: bool,
    /// 已拉起但端口还没起来（服务启动需要一点时间）。
    starting: bool,
    pid: Option<u32>,
    port: u16,
    base_url: String,
    /// 解析到的可执行文件路径（可能为 None：没找到）。
    exe: Option<String>,
    exe_exists: bool,
    /// 密钥/令牌的存放目录（`state.json` 所在处）。定位不到 exe 时为 None。
    data_dir: Option<String>,
    /// 找不到可执行文件时列出尝试过的位置，方便用户手动指定。
    searched: Vec<String>,
    /// 本平台是否提供 Python 反代服务old。Python 版只随 Windows 安装包分发
    /// （tauri.windows.conf.json 的 bundle 资源），其它平台返回 false，
    /// 前端据此隐藏菜单与启动入口。
    supported: bool,
    first_run_password: Option<String>,
    log_tail: Vec<String>,
}

// ── 配置持久化 ────────────────────────────────────────────────────────
//
// 复用 switch 自己的 store 目录（`~/.wb-switch-plus/`），不另立门户。

fn config_path() -> PathBuf {
    config::store_dir().join("reverse_proxy.json")
}

fn load_saved() -> Value {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn save_saved(value: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    config::atomic_write(&config_path(), &text)
}

fn saved_exe() -> Option<PathBuf> {
    load_saved()
        .get("exe")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(PathBuf::from)
}

// ── 退出行为：自动关闭反代服务 ──────────────────────────────────────────

/// 「关闭软件时自动关闭反代服务」的配置键。
const CLOSE_ON_EXIT_KEY: &str = "close_on_exit";

/// 读取退出开关。**默认 false**（保持既有行为）。
///
/// 反代服务是独立可执行文件，本来就设计成「关掉桌面端它仍然服务客户端」
/// （见本文件顶部注释）。所以这个开关必须**显式打开**才会改变行为 ——
/// 默认改成 true 会让升级上来的老用户「关掉桌面端，客户端就断了」。
pub fn close_on_exit() -> bool {
    load_saved()
        .get(CLOSE_ON_EXIT_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// 写入退出开关并**回读**，返回落盘后的真实值。
///
/// 回读而不是直接回显入参：`atomic_write` 失败时用户界面上会显示「已开启」，
/// 而下次退出根本不会关服务 —— 那种「设置像是存住了其实没存」最难查。
fn set_close_on_exit(enabled: bool) -> Result<bool, String> {
    let mut saved = load_saved();
    if !saved.is_object() {
        saved = json!({});
    }
    saved[CLOSE_ON_EXIT_KEY] = json!(enabled);
    save_saved(&saved).map_err(|error| format!("保存退出设置失败：{error}"))?;
    Ok(close_on_exit())
}

#[tauri::command]
pub fn reverse_proxy_close_on_exit() -> bool {
    close_on_exit()
}

#[tauri::command]
pub fn reverse_proxy_set_close_on_exit(enabled: bool) -> Result<bool, String> {
    set_close_on_exit(enabled)
}

/// 进程退出时按开关关闭反代服务。
///
/// 在 `RunEvent::Exit`（而不是 `ExitRequested`）里调用：`Exit` 是所有退出路径
/// 的**汇合点** —— 托盘「退出应用」、窗口关闭、系统关机都会走到它。
/// 挂在 `ExitRequested` 上会漏掉托盘那条路（那里直接调 `app.exit(0)`）。
///
/// 关不上也**绝不能 panic 或阻塞退出**：用户点的是「退出应用」，
/// 卡在关服务的等待里比留着服务更糟。所以全部失败路径都静默吞掉。
pub fn shutdown_if_configured(state: &ReverseProxy) {
    if !close_on_exit() {
        return;
    }
    // 复用 stop 的收尾逻辑（含「PyInstaller onefile 的监听者是子进程」那条处理）。
    // 返回值是「关掉了没有」的报告，这里不需要：退出路径上没有界面可显示，
    // 也不能因为关不掉就拦住退出。
    let _ = stop_service(state);
}

// ── 定位可执行文件 ────────────────────────────────────────────────────

fn exe_name() -> String {
    if cfg!(windows) {
        "wb-reverse-proxy.exe".to_string()
    } else {
        "wb-reverse-proxy".to_string()
    }
}

/// 按优先级列出候选路径：
/// 1. 用户手动指定的（配置文件里的 `exe`）
/// 2. Tauri 资源目录——打包后 exe 随安装包分发，这是正常安装下的位置
/// 3. 与桌面端可执行文件同级——绿色版（解压即用）会把它放在一起
/// 4. 源码树里的构建产物 `reverse-proxy/dist/`——开发态
fn candidates(saved: Option<&Path>, resource_dir: Option<&Path>) -> Vec<PathBuf> {
    let name = exe_name();
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(path) = saved {
        out.push(path.to_path_buf());
    }
    if let Some(dir) = resource_dir {
        // 资源清单里把它放在根，所以直接在资源目录下找；同时也试一下子目录，
        // 以防将来改成保留原始层级。
        out.push(dir.join(&name));
        out.push(dir.join("resources").join(&name));
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            out.push(dir.join(&name));
        }
    }
    // 源码树里的构建产物 `reverse-proxy/dist/`——**只对开发态有效**。
    //
    // release 里绝不能留这条：`env!("CARGO_MANIFEST_DIR")` 是编译期的绝对路径，
    // 会原样烧进二进制。用户在别的机器上装好桌面端后，一旦前三个候选都没命中
    // （比如随包分发的 exe 被删了），就会静默去读「打包这台机器的源码树」里的那份，
    // 于是出现「装的是 C 盘，跑的反代却来自某个开发目录」而且完全看不出原因。
    #[cfg(debug_assertions)]
    {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        out.push(
            manifest
                .join("..")
                .join("reverse-proxy")
                .join("dist")
                .join(&name),
        );
    }
    out
}

fn resolve(saved: Option<PathBuf>, resource_dir: Option<&Path>) -> (Option<PathBuf>, Vec<String>) {
    let tried = candidates(saved.as_deref(), resource_dir);
    let found = tried.iter().find(|path| path.is_file()).cloned();
    let tried = tried
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    (found, tried)
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

// ── 状态 ──────────────────────────────────────────────────────────────

impl ReverseProxy {
    /// 记下 Tauri 的资源目录，供 `candidates()` 在打包后的安装目录里找 exe。
    ///
    /// 必须在 setup 阶段调用：`resource_dir()` 只有拿到 app handle 之后才可解析。
    pub fn set_resource_dir(&self, dir: Option<PathBuf>) {
        self.lock().resource_dir = dir;
    }

    /// `Mutex` 中毒也要能继续用：这里持的都是纯数据，没有「半更新」的不变量。
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> Status {
        let mut inner = self.lock();
        status_of(&mut inner)
    }
}

fn status_of(inner: &mut Inner) -> Status {
    // 子进程可能已经退出（端口被占、启动报错等），借这次调用回收句柄。
    let mut managed = false;
    if let Some(child) = inner.child.as_mut() {
        match child.try_wait() {
            Ok(None) => managed = true,
            _ => {
                inner.child = None;
                inner.pid = None;
            }
        }
    }

    let port = if inner.port == 0 { DEFAULT_PORT } else { inner.port };
    let running = port_open(port);

    let exe = inner
        .exe
        .clone()
        .or_else(|| resolve(saved_exe(), inner.resource_dir.as_deref()).0);
    let (resolved_exists, searched) = match exe.as_ref() {
        Some(path) => (path.is_file(), Vec::new()),
        None => {
            let (_, tried) = resolve(None, inner.resource_dir.as_deref());
            (false, tried)
        }
    };

    let log_tail = inner
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let first_run_password = inner
        .password
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    // 数据目录是应用自己的属性，跟能不能定位到服务 exe 无关，永远给得出。
    let data_dir = data_dir();

    Status {
        running,
        managed,
        starting: managed && !running,
        pid: inner.pid,
        port,
        base_url: format!("http://127.0.0.1:{port}"),
        exe: exe.map(|path| path.display().to_string()),
        exe_exists: resolved_exists,
        data_dir: Some(data_dir.display().to_string()),
        searched,
        supported: legacy_service_supported(),
        first_run_password,
        log_tail,
    }
}

// ── 日志读取 ──────────────────────────────────────────────────────────

/// 把子进程的一条输出流接进环形缓冲；`stderr` 流顺便抓结构化横幅。
fn pump(mut stream: impl Read + Send + 'static, inner_log: Arc<Mutex<Vec<String>>>, password: Arc<Mutex<Option<String>>>, watch_banner: bool) {
    std::thread::spawn(move || {
        let reader = BufReader::new(&mut stream);
        for line in reader.lines().map_while(Result::ok) {
            if watch_banner {
                if let Some(rest) = line.trim().strip_prefix(BANNER_PREFIX) {
                    if let Some(value) = rest.trim().strip_prefix("first_run_password=") {
                        let mut guard = password
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        *guard = Some(value.trim().to_string());
                    }
                }
            }
            let mut guard = inner_log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.push(line);
            if guard.len() > LOG_LIMIT {
                let overflow = guard.len() - LOG_LIMIT;
                guard.drain(0..overflow);
            }
        }
    });
}

// ── 命令 ──────────────────────────────────────────────────────────────

#[tauri::command]
pub fn reverse_proxy_status(state: tauri::State<'_, ReverseProxy>) -> Status {
    state.snapshot()
}

#[tauri::command]
pub fn reverse_proxy_start(
    state: tauri::State<'_, ReverseProxy>,
    port: Option<u16>,
    host: Option<String>,
) -> Result<Status, String> {
    if !legacy_service_supported() {
        return Err(
            "Python 反代服务old 只随 Windows 版分发，当前平台不可用。内置反代（Rust 版）无需额外组件。"
                .to_string(),
        );
    }
    start_service(&state, port, host)
}

/// 启动服务的**唯一实现**。命令与「以 0.0.0.0 重启」共用它。
///
/// `host` 为 `None` 时**不传 `--host`**，让服务走它自己的默认值（127.0.0.1），
/// 保持既有行为完全不变 —— 只有用户显式点了「对局域网开放」才绑 0.0.0.0。
fn start_service(
    state: &ReverseProxy,
    port: Option<u16>,
    host: Option<String>,
) -> Result<Status, String> {
    let mut inner = state.lock();

    let port = match port.map(u16::from).filter(|value| *value > 0) {
        Some(value) => value,
        None => inner.port.max(1),
    };
    inner.port = port;

    // 只接受这两种地址。允许任意字符串会把 `--host` 变成参数注入面
    // （比如传进 `--data` 之类的额外参数）。
    let host = match host.as_deref().map(str::trim) {
        None | Some("") => None,
        Some("0.0.0.0") => Some("0.0.0.0".to_string()),
        Some("127.0.0.1") => Some("127.0.0.1".to_string()),
        Some(other) => {
            return Err(format!(
                "监听地址只能是 127.0.0.1 或 0.0.0.0，收到的是 {other}"
            ))
        }
    };

    // 端口已经有人监听（可能是用户自己起的服务）就不再重复拉起。
    if port_open(port) {
        return Ok(status_of(&mut inner));
    }

    // 每次启动都重新解析：用户可能刚把 exe 拷进来，或刚在界面里改过路径。
    let (exe, searched) = resolve(saved_exe(), inner.resource_dir.as_deref());
    let Some(exe) = exe else {
        return Err(format!(
            "没找到反代服务可执行文件。请构建 reverse-proxy（python build-exe.py），\
             或在界面里手动指定。已尝试的位置：\n{}",
            searched.join("\n")
        ));
    };

    // 数据目录归到 `~/.wb-switch-plus/reverse-proxy`，并把老的「exe 同级 data/」搬过来。
    let data_dir = data_dir();
    if let Err(error) = std::fs::create_dir_all(&data_dir) {
        return Err(format!("创建数据目录 {} 失败：{error}", data_dir.display()));
    }
    let adopted = adopt_legacy_data(&exe, &data_dir);

    let mut command = Command::new(&exe);
    command
        .arg("--port")
        .arg(port.to_string())
        .arg("--data")
        .arg(&data_dir);
    // 显式绑 0.0.0.0 时才传 --host：对外开端口是用户主动做的决定，
    // 不能因为「界面上点过一次」就永久生效，所以不写进 saved。
    if let Some(host) = &host {
        command.arg("--host").arg(host);
    }
    // 记下这次用的监听地址：换端口重启时要沿用它，见 Inner::host 的注释。
    inner.host = host.clone();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    // 服务由桌面端托管，不弹控制台窗口；输出通过管道进日志面板。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("启动反代服务失败：{error}"))?;

    inner.log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    *inner.password.lock().unwrap_or_else(|p| p.into_inner()) = None;

    // 绑定非本机地址是一条安全姿态的变更，必须在日志里留痕：
    // 用户下次看到「怎么外网能访问」时，这里是唯一的线索。
    if let Some(host) = &host {
        if host == "0.0.0.0" {
            inner.log.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
                "[WB-Switch-Plus] 已绑定 0.0.0.0:{port}，本机所在网络内的其他设备可以访问。\
                 若不需要，请点「只绑本机」改回来。"
            ));
        }
    }

    // 归位提示写进日志面板：这是用户只会在第一次启动时看到的一次性动作，
    // 不提示的话「管理口令变了没有」会变成一个说不清的问题。
    if let Some(legacy) = adopted {
        inner.log.lock().unwrap_or_else(|p| p.into_inner()).push(format!(
            "[WB-Switch-Plus] 已把旧的密钥数据从 {} 搬到 {}",
            legacy.display(),
            data_dir.display()
        ));
    }

    if let Some(stdout) = child.stdout.take() {
        pump(stdout, inner.log.clone(), inner.password.clone(), false);
    }
    if let Some(stderr) = child.stderr.take() {
        pump(stderr, inner.log.clone(), inner.password.clone(), true);
    }

    inner.pid = Some(child.id());
    inner.child = Some(child);
    inner.exe = Some(exe);
    drop(inner);

    let mut saved = load_saved();
    saved["port"] = json!(port);
    let _ = save_saved(&saved);

    Ok(state.snapshot())
}

/// 以指定监听地址**重启**反代服务。
///
/// 用「先停后启」而不是改配置再热重载：服务的监听地址是 uvicorn 启动参数，
/// 进程起来之后无法更改，重启是唯一的实现方式。停止走 `stop_service`，
/// 因此「按端口反查监听者」那套收尾逻辑（PyInstaller onefile 必需）同样生效。
#[tauri::command]
pub fn reverse_proxy_restart_with_host(
    state: tauri::State<'_, ReverseProxy>,
    host: String,
) -> Result<Status, String> {
    let port = {
        let inner = state.lock();
        if inner.port == 0 {
            DEFAULT_PORT
        } else {
            inner.port
        }
    };
    stop_service(&state)?;
    start_service(&state, Some(port), Some(host))
}

/// 换端口重启。
///
/// 端口同样是 uvicorn 的启动参数，改不了只能重启，所以走的也是「先停后启」。
/// 与「换监听地址」的区别：**新端口要先确认没被占**。被占时若先停再启，
/// 结果是旧服务已经没了、新服务又起不来 —— 用户从「能用」掉到「完全不能用」，
/// 而这本来是一次可以拒绝的请求。
///
/// 监听地址**必须沿用当前姿态**：只在 127.0.0.1 上换端口，不该顺手把服务
/// 暴露到局域网。判断依据是状态里的 baseUrl 是否已经绑了 0.0.0.0。
#[tauri::command]
pub fn reverse_proxy_restart_with_port(
    state: tauri::State<'_, ReverseProxy>,
    port: u16,
) -> Result<Status, String> {
    if port < 1 {
        return Err("端口必须大于 0".to_string());
    }
    let (current, exposed) = {
        let inner = state.lock();
        let current = if inner.port == 0 { DEFAULT_PORT } else { inner.port };
        (current, inner.host.as_deref() == Some("0.0.0.0"))
    };
    if port == current {
        // 没变就什么都不做。这里**故意不重启**：用户手滑点两下不该让服务抖两次，
        // 正在转发的请求也不该因为这个失败。
        let mut inner = state.lock();
        return Ok(status_of(&mut inner));
    }
    if port_open(port) {
        return Err(format!(
            "端口 {port} 已被占用，换端口前请先释放它（当前服务仍运行在 {current} 上，未受影响）"
        ));
    }
    stop_service(&state)?;
    start_service(&state, Some(port), exposed.then(|| "0.0.0.0".to_string()))
}

/// 服务数据目录：密钥、管理令牌、state.json 都在这里。
///
/// 冻结形态下服务自己的默认值是「exe 同级 data/」（见 `reverse-proxy/serve.py`
/// 的 `default_data_root()`），但**桌面端托管时一律显式传 `--data` 指到这里**：
/// 本仓库是二改，所有落盘的东西都要归到 `~/.wb-switch-plus` 名下，
/// 否则用户会把反代数据留在一堆 `target/release/data` 里找不到。
/// 服务被用户手动双击运行时仍走它自己的 exe 同级默认值（绿色运行需要这种可移动性）。
fn data_dir() -> PathBuf {
    config::store_dir().join("reverse-proxy")
}

/// 首次归位时把「exe 同级 data/」里的老状态搬过来，别让管理员密码和签到记录白丢。
///
/// 只在新目录还没有 state.json 时执行一次；老目录不存在就什么都不做。
fn adopt_legacy_data(exe: &Path, target: &Path) -> Option<PathBuf> {
    if target.join("state.json").is_file() {
        return None;
    }
    let legacy = exe.parent()?.join("data");
    if !legacy.join("state.json").is_file() {
        return None;
    }
    if copy_dir(&legacy, target).is_err() {
        return None;
    }
    Some(legacy)
}

/// 递归复制目录（只复制文件，不跟随符号链接）。目标目录会被创建。
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest)?;
        }
    }
    Ok(())
}

#[tauri::command]
pub fn reverse_proxy_stop(state: tauri::State<'_, ReverseProxy>) -> Result<Status, String> {
    stop_service(&state)
}

/// 停止服务的**唯一实现**。命令与退出钩子共用它。
///
/// 从 `reverse_proxy_stop` 的命令体里抽出来，是为了让「退出时自动关闭」走上
/// **完全相同**的收尾逻辑。若退出钩子自己写一份「杀子进程」的简化版，就会漏掉
/// 下面那段「按端口反查监听者」—— 而那恰恰是 PyInstaller onefile 打包后
/// 唯一有效的路径（真正的监听者是解出来的子进程，外层父进程只是壳）。
/// 漏掉它，用户打开「自动关闭」后会发现服务**照常活着**。
fn stop_service(state: &ReverseProxy) -> Result<Status, String> {
    let mut inner = state.lock();
    let port = if inner.port == 0 { DEFAULT_PORT } else { inner.port };

    // 桌面端自己拉起的服务：先结束它。
    if let Some(mut child) = inner.child.take() {
        let pid = inner.pid.take();
        #[cfg(windows)]
        if let Some(pid) = pid {
            let _ = crate::winproc::terminate(pid);
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    // 收尾一律按「谁在监听这个端口」来：
    // · PyInstaller onefile 的监听者是解出来的子进程，只杀外层父进程会留下孤儿占端口；
    // · 服务也可能压根不是本次桌面端拉起的（用户自己双击了 exe，或上次退出后它一直在跑），
    //   那种情况下手里没有子进程句柄，只能反查。
    #[cfg(windows)]
    if port_open(port) {
        let expected = exe_name();
        let mut foreign: Vec<u32> = Vec::new();
        for pid in crate::winproc::listening_pids(port) {
            match crate::winproc::image_name_of(pid) {
                // 名字读不到也结束：能查出监听者就是本机进程，这个端口本来就是给反代用的。
                None => {
                    let _ = crate::winproc::terminate(pid);
                }
                Some(name) if name.eq_ignore_ascii_case(&expected) => {
                    let _ = crate::winproc::terminate(pid);
                }
                // 名字明确不是反代服务（用户把端口填成了别的程序）→ 不动它。
                Some(_) => foreign.push(pid),
            }
        }
        for _ in 0..20 {
            if !port_open(port) {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        if port_open(port) && !foreign.is_empty() {
            return Err(format!(
                "端口 {port} 上监听的不是反代服务（进程 {foreign:?}），已保留未动。\
                 请先在「服务程序」里确认端口，或改用其它端口。"
            ));
        }
    }

    let status = status_of(&mut inner);
    if status.running {
        return Err(format!(
            "{} 仍在监听，没能停掉。若该进程属于其它用户或更高权限，\
             可以点「以管理员身份停止」。",
            status.base_url
        ));
    }
    Ok(status)
}

/// 以管理员身份结束占用端口的服务（会弹 UAC）。
///
/// 用「重新拉起自己 + `--stop-pid=`」而不是 `taskkill`：部分机器上 taskkill 直接
/// 不可用（见 `winproc` 模块头注释），这样也不依赖目标机装了什么工具。
#[tauri::command]
pub fn reverse_proxy_stop_elevated(state: tauri::State<'_, ReverseProxy>) -> Result<String, String> {
    #[cfg(windows)]
    {
        let port = {
            let inner = state.lock();
            inner.port.max(1).max(if inner.port == 0 { DEFAULT_PORT } else { inner.port })
        };
        let managed = { state.lock().pid };
        let pid = match managed.filter(|pid| *pid != 0) {
            Some(pid) => pid,
            None => match crate::winproc::listening_pids(port).first() {
                Some(pid) => *pid,
                None => return Err(format!("端口 {port} 上没有找到监听进程，服务可能已经停了。")),
            },
        };
        crate::winproc::elevate_stop_pid(pid)?;
        return Ok(format!("已请求以管理员身份结束进程 {pid}，请在 UAC 弹窗里允许。"));
    }
    #[cfg(not(windows))]
    {
        Err("非 Windows 平台不需要提权结束。".to_string())
    }
}

/// 打开「密钥储存位置」——在资源管理器里定位到 state.json。
#[tauri::command]
pub fn reverse_proxy_open_data_dir() -> Result<String, String> {
    let dir = data_dir();
    // 服务还没起过时这个目录可能不存在，Explorer 打不存在的路径会退回「文档」。
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建数据目录失败：{error}"))?;

    #[cfg(windows)]
    {
        // 有 state.json 就选中它，没有就开目录。
        let state_file = dir.join("state.json");
        let mut command = Command::new("explorer");
        if state_file.is_file() {
            command.arg("/select,").arg(&state_file);
        } else {
            command.arg(&dir);
        }
        command.spawn().map_err(|error| format!("打开目录失败：{error}"))?;
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        Command::new(opener)
            .arg(&dir)
            .spawn()
            .map_err(|error| format!("打开目录失败：{error}"))?;
    }
    Ok(dir.display().to_string())
}

// ── 记住登录凭据 ──────────────────────────────────────────────────────

/// 保存的登录凭据文件（与 `reverse_proxy.json` 同目录）。
///
/// 明文存放：这一版是单机自用，`state.json` 里本来就躺着账号凭据与 API 密钥，
/// 单独给这个文件套一层加密只会给人「比实际更安全」的错觉。界面上会写明存在哪里。
fn login_path() -> PathBuf {
    config::store_dir().join("reverse-proxy-login.json")
}

#[tauri::command]
pub fn reverse_proxy_saved_login() -> Option<Value> {
    let text = std::fs::read_to_string(login_path()).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.is_object().then_some(value)
}

/// 保存或清除登录凭据。传 `None`（或没有 `mode` 的对象）= 清除。
#[tauri::command]
pub fn reverse_proxy_save_login(login: Option<Value>) -> Result<(), String> {
    let path = login_path();
    let keep = login.as_ref().is_some_and(|value| {
        value.is_object()
            && value
                .get("mode")
                .and_then(Value::as_str)
                .is_some_and(|mode| !mode.is_empty())
    });
    if !keep {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("清除登录凭据失败：{error}")),
        };
    }
    let text = serde_json::to_string_pretty(&login.expect("keep 为真时必有值"))
        .map_err(|error| format!("序列化登录凭据失败：{error}"))?;
    config::atomic_write(&path, &text).map_err(|error| format!("保存登录凭据失败：{error}"))
}

/// 手动指定反代服务可执行文件；传空字符串表示清除指定、回到自动探测。
///
/// 这个路径会被 `reverse_proxy_start` 直接 `Command::new` 起来，
/// 所以只判断「文件存在」是不够的 —— 那等于任意路径都能成为执行目标。
#[tauri::command]
pub fn reverse_proxy_set_exe(
    state: tauri::State<'_, ReverseProxy>,
    path: Option<String>,
) -> Result<Status, String> {
    let cleaned = path
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(value) = cleaned.as_deref() {
        validate_custom_exe(value)?;
    }

    let mut saved = load_saved();
    saved["exe"] = json!(cleaned.clone().unwrap_or_default());
    save_saved(&saved).map_err(|error| format!("保存配置失败：{error}"))?;

    let mut inner = state.lock();
    inner.exe = cleaned.map(PathBuf::from);
    Ok(status_of(&mut inner))
}

/// 校验手动指定的反代 exe 能否作为执行目标。
///
/// 允许手动指定是因为用户可能把反代放在任意位置，只挡掉明确不该被执行的目标：
/// 非可执行文件、系统目录、UNC 网络路径、相对路径。
fn validate_custom_exe(value: &str) -> Result<(), String> {
    let candidate = Path::new(value);
    if !candidate.is_file() {
        return Err(format!("指定的文件不存在：{value}"));
    }

    if !candidate.is_absolute() {
        return Err("请填写完整路径（不支持相对路径）".to_string());
    }

    #[cfg(windows)]
    {
        let ext = candidate
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "exe" {
            return Err("反代服务必须是 .exe 文件".to_string());
        }
        // UNC：\\server\share\...
        if value.starts_with(r"\\") {
            return Err("不支持网络共享路径，请先把文件复制到本机".to_string());
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            let system32 = PathBuf::from(&root).join("System32");
            if candidate.starts_with(&system32) {
                return Err("不支持系统目录下的程序".to_string());
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ok = std::fs::metadata(candidate)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if !ok {
            return Err("反代服务没有可执行权限".to_string());
        }
        for dir in ["/bin", "/sbin", "/usr/bin", "/usr/sbin", "/System"] {
            if candidate.starts_with(dir) {
                return Err("不支持系统目录下的程序".to_string());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_lives_under_the_plus_store() {
        // 二改的规矩：所有落盘的东西都在 ~/.wb-switch-plus 名下，不再有 exe 同级 data/。
        let path = data_dir();
        assert_eq!(path.file_name().unwrap(), "reverse-proxy");
        assert_eq!(path.parent().unwrap(), config::store_dir().as_path());
        assert!(path.starts_with(config::store_dir()));
    }

    #[test]
    fn adopt_legacy_data_copies_beside_exe_state_once() {
        let root = std::env::temp_dir().join(format!("wbsp-adopt-{}", std::process::id()));
        let exe = root.join("app").join("wb-reverse-proxy.exe");
        let legacy = exe.parent().unwrap().join("data");
        let target = root.join("store").join("reverse-proxy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("state.json"), "{\"users\":[]}").unwrap();

        // 第一次：搬过去，并回报老目录。
        assert_eq!(adopt_legacy_data(&exe, &target).as_deref(), Some(legacy.as_path()));
        assert!(target.join("state.json").is_file());

        // 第二次：新目录已有 state.json，不再动手（也不覆盖）。
        std::fs::write(target.join("state.json"), "{\"users\":[1]}").unwrap();
        assert!(adopt_legacy_data(&exe, &target).is_none());
        assert_eq!(std::fs::read_to_string(target.join("state.json")).unwrap(), "{\"users\":[1]}");

        // 老目录不存在 → 什么都不做，不报错。
        let other_exe = root.join("other").join("wb-reverse-proxy.exe");
        assert!(adopt_legacy_data(&other_exe, &target).is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn login_path_lives_in_the_switch_store() {
        let path = login_path();
        assert_eq!(path.file_name().unwrap(), "reverse-proxy-login.json");
        assert_eq!(path.parent().unwrap(), config::store_dir().as_path());
    }

    #[test]
    fn validate_custom_exe_rejects_non_executable_and_system_paths() {
        let root = std::env::temp_dir().join(format!("wbsp-exe-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        // 不存在 / 不是文件 -> 拒
        assert!(validate_custom_exe(&root.join("nope.exe").to_string_lossy()).is_err());
        assert!(validate_custom_exe(&root.to_string_lossy()).is_err());

        // 相对路径 -> 拒（解析基准不确定，容易被同名文件顶替）
        assert!(validate_custom_exe("wb-reverse-proxy.exe").is_err());

        #[cfg(windows)]
        {
            // 存在但不是 .exe -> 拒
            let txt = root.join("payload.txt");
            std::fs::write(&txt, b"x").unwrap();
            assert!(validate_custom_exe(&txt.to_string_lossy()).is_err());

            // UNC 路径 -> 拒
            assert!(validate_custom_exe(r"\\server\share\evil.exe").is_err());

            // 系统目录 -> 拒（System32 下没有合法反代）
            if let Some(sysroot) = std::env::var_os("SystemRoot") {
                let victim = PathBuf::from(sysroot)
                    .join("System32")
                    .join("cmd.exe");
                if victim.is_file() {
                    assert!(validate_custom_exe(&victim.to_string_lossy()).is_err(),
                            "System32 下的程序不该被接受为反代");
                }
            }

            // 合法的用户目录 exe -> 放行
            let ok = root.join("wb-reverse-proxy.exe");
            std::fs::write(&ok, b"x").unwrap();
            assert!(validate_custom_exe(&ok.to_string_lossy()).is_ok(),
                    "用户自己目录里的 .exe 是合法用法，不该被挡");
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
