# CODEBUDDY.md

This file provides guidance to CodeBuddy Code when working with code in this repository.

## 项目概览

WB-Switch-Plus（`workbuddy-switch-plus`）是 WorkBuddy / CodeBuddy IDE / CodeBuddy CLI / VS Code / JetBrains 插件的账号切换桌面应用。技术栈是 Tauri 2 + Rust 后端 + React 19 前端，外加一个独立的 Python FastAPI 反代服务。仓库文档与注释以中文为主，新增注释沿用中文。

## 常用命令

```bash
npm install
npm run dev                 # 只跑前端（:1420，无后端）；webui 数据靠同源 /api，通常先 build 再跑 wb-switch 服务
npm run dev:desktop         # 准备 Companion 后跑 vite
npm run tauri dev           # 桌面端开发（Rust + 前端热更新）
npm run dev:demo            # 虚构数据的演示模式（VITE_DEMO_MODE=1）

npm run build               # 清 Companion 演示产物 + tsc + vite build（webui 静态资源 → dist/）
npm run build:desktop       # 前端 + Companion + agent-studio-runtime，桌面端打包前置
npm run tauri build         # 生成安装包（Windows 脚本用 --bundles nsis）

sh scripts/build-win-x64.sh # Windows 一键构建（--check 只做前置自检）
npm run verify:release      # 打包后自检：产物里不能残留构建机绝对路径
```

测试（三套独立体系，互不覆盖）：

```bash
npm test                                    # vitest run，只匹配 src/**/*.test.ts
npx vitest run src/lib/variant.test.ts      # 单个前端测试
cargo test -p wb-switch-core                # Rust 核心逻辑（各模块内含 #[test]）
cargo test -p wb-switch-core --test cli_auth_sync_e2e
cd reverse-proxy && python -m pytest tests/test_account_pool.py
```

反代服务（Python，改动 `reverse-proxy/**.py` 后必须重打包，exe 不会自动更新）：

```bash
cd reverse-proxy && python serve.py --port 8787    # 本地运行
pip install -r requirements.txt pyinstaller && python build-exe.py   # → dist/wb-reverse-proxy.exe
```

## 架构

### 核心逻辑下沉到 wb-switch-core

`crates/wb-switch-core` 是不依赖 Tauri 的核心库（账号库、认证文件、OAuth、进程探测、切换、会话、签到、轮换、更新、配置等，全部在 `src/modules/` 下）。它被两个宿主复用：

- `src-tauri/`：桌面端。`commands.rs` 与 `lib.rs` 里的 `#[tauri::command]` 只是**薄包装**，业务逻辑一律下沉到 core；`#[cfg]` 分支承载 Windows / macOS / Linux 差异（Windows 走 Win32 直调与注册表，macOS 走文件权限 / TCC 引导，Linux 走 zbus D-Bus Secret Service）。
- `crates/wb-switch-proxy`：内置反代。桌面端启动时由 `src-tauri/src/proxy_service.rs` 拉起进程内 axum 监听（`127.0.0.1:8787` 起，冲突自动 +1），管理面凭宿主令牌鉴权。

新增一个功能时，逻辑写进 core，再在 `commands.rs` 加一层薄适配。反代侧的管理面新增在 `crates/wb-switch-proxy/src/admin.rs`。

> 历史上有第三个宿主 `crates/wb-switch-server`（浏览器 webui，npm 安装形态），已随浏览器形态一起移除——现在只有 Tauri 桌面端一个宿主。

### 前端单通道

`src/lib/api.ts` 是后端适配层，只走 `invoke`。页面/组件不直接调用 `invoke`，新增接口在 `api.ts` 里加一个函数。例外是反代管理面：`src/lib/reverse-proxy.ts` 直接 `fetch` 内置反代的 `/admin/api/*`（地址与宿主令牌由 `proxy_service_status` / `proxy_admin_token` 注入，见 `setHostProvider`），`src/lib/reverse-proxy-service.ts` 直接 `invoke` Python old 的启动器命令。`src/lib/types.ts` 定义与 Rust 侧对应的类型，`src/stores/accounts.ts`（zustand）持有跨页面复用的账号、档位与各客户端状态。

页面在 `src/pages/`（账号 / 会话 / 积分统计 / Token 统计 / 反代 / 设置；反代是 `src/pages/reverse-proxy/` 下的路由组），路由定义在 `src/App.tsx`。

### 国内版 / 国际版档位

`WbVariant`（`cn` / `ai`）贯穿前后端。前端判定与展示名统一收敛在 `src/lib/variant.ts`（`variantLabel` / `variantSupportsCheckin` 等）；新代码不要再散写档位字符串，改造旧代码时顺手收敛（`AccountsPage` 等处仍有直接的 `variant === "ai"`）。后端用 `variant::WbVariant::parse`。缺省一律国内版。部分能力（签到、成长中心旅行）只在国内版开放。

### 数据目录

运行时数据在 `~/.wb-switch-plus/`（账号库 `accounts.json`、会话关联、hook 配置、错误日志）；桌面端托管反代时状态在 `~/.wb-switch-plus/reverse-proxy/state.json`（独立跑 `serve.py` 是 `reverse-proxy/data/`，单独跑 exe 是 exe 同级 `data/`）。`WB_SWITCH_HOME` 可覆盖 home 基准目录。首次启动会从上游 `~/.wb-switch` 整份复制（`config::migrate_legacy_store`，在 `lib.rs` 的 `run()` 最开始调用），必须先于任何模块读账号库。

### 反代服务

`reverse-proxy/` 是独立 Python 进程（FastAPI + uvicorn），把本机账号库转成 OpenAI / Anthropic 兼容接口，含账号池轮换与多密钥分发。桌面端只做「启动器」：`src-tauri/src/reverse_proxy.rs` 按「用户指定 → 资源目录 → exe 同级 → 源码树（仅 debug）」顺序找可执行文件，进程句柄不落盘，重启后靠端口探测重新识别。协议转换在 `reverse-proxy/core/`，管理面在 `reverse-proxy/admin/`。

同一套能力已内迁到 `crates/wb-switch-proxy`（进程内 axum），由 `src-tauri/src/proxy_engine.rs` 托管，灰度期用前端「引擎」切换器二选一。

**账号只有一份**：两个引擎都读 `~/.wb-switch-plus/accounts.json`，池只是它的一层属性。可用性判据固定为
`paused → invalid → cooling → exhausted → available`，其中 `invalid` = `needs_relogin`（桌面端刷新失败写入）| `auth_invalid` | 无 token。
Rust `pool.rs` 与 Python `pool.state()` 必须逐项一致——历史上只看 `access_token` 非空，导致「账户管理说需重新登录、反代页说可用」。
账号的增删与登录只在账户管理做；反代页的「扫码登录」在桌面端直接复用主项目 OAuth（写同一份库），不要另实现一套。

### Agent Companion 悬浮窗

`vendor/agent-companion/` 是 vendored 的 `tauri-plugin-agent-studio`，由 `scripts/prepare-agent-companion.mjs` 准备 `dist-embed` 产物后再编译进桌面端。这些 embed 产物是生成物，不进仓库。

## 容易踩的坑

- `src-tauri/Cargo.toml` 默认启用 `custom-protocol` feature，这是有意为之：不开它的 release 构建会指向 devUrl，窗口直接报「localhost 拒绝连接」。不要移除；`tauri dev` 传 `--no-default-features` 也是配套设计。
- 根 `Cargo.toml` 用 `[patch.crates-io]` 把 `indexmap` 指向 `vendor-patched/indexmap`，绕过新版 Rust 上 `indexmap 1.9.3` 的 `E0107`。这是构建层修复，与业务无关，不要当成依赖清理任务删掉。
- 反代 exe 是 `tauri.windows.conf.json` 里声明的 bundle resource，文件不存在会让 Tauri 构建直接硬报错；因此该声明**只能**在 Windows 配置里，基线 `tauri.conf.json` 不能有。
- `productName`（`WB-Switch-Plus`）改名会连带影响进程映像名。`crates/wb-switch-core/src/modules/process.rs` 的 `SELF_IMAGE_STEMS` 是精确（忽略大小写、非子串）匹配清单，目前含 `wb-switch-plus` / `wb-switch-rust` / `workbuddy-switch` / `wb-switch` 四项，漏改会导致「关闭客户端」时把自己的进程也算进去。
- 不要用 `env!("CARGO_MANIFEST_DIR")` 做运行时路径兜底（会泄漏构建机绝对路径），只能用 `include_bytes!` 或 `#[cfg(debug_assertions)]` 包住。`npm run verify:release` 就是查这个。
- 两个反代引擎共用同一份 `state.json`。桌面端 Rust 引擎用 `StateStore::open_writable`（只读会让池设置/密钥保存直接 403），
  `update()` 落盘前会先重读磁盘避免整份覆盖，但**同秒写仍是后写者胜**——灰度期只能启用一个引擎。
- 演示/截图模式（前端 `VITE_DEMO_MODE` / `VITE_SCREENSHOT_DEMO`，桌面端 `WB_SWITCH_SCREENSHOT_DEMO`）必须完全不读真实账号、不触发签到/轮换/保活；新增读取接口时要同时补 `api.ts` 的 `DEMO_READ_COMMANDS` 与 fixture。
- 仓库不提交本地数据与凭据（账号库、认证文件、密钥、token 均在 `.gitignore` 中）；发版前用 `git grep` 扫 `ghp_` / `npm_` / `gho_` 等模式。

## 参考文档

- `docs/DEVELOPMENT.md` —— 发版流程、签名密钥、四平台 CI、反代随包分发细节
- `reverse-proxy/README.md` 与 `reverse-proxy/PORTING.md` —— 反代协议与服务改造清单
- `docs/agent-companion-integration.md` —— 悬浮窗接入说明
