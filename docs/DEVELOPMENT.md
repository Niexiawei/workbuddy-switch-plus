# 开发指南

## 环境要求

Node.js ≥ 20、Rust stable、macOS（或 Windows/Linux）。

## 开发命令

```bash
npm install
npm run tauri dev        # 开发模式
npm run build:app        # 构建 debug .app（含前端资源补丁）
npm run build:app:release  # 构建 release .app + 签名更新包
```

## 发布新版本

签名密钥（自动更新用）存放于 `~/.wb-switch-plus/wb-switch-updater.key`，构建脚本通过
`TAURI_SIGNING_PRIVATE_KEY` 注入。

### 四平台自动发版（GitHub Actions）

`.github/workflows/build.yml` 在推 `v*` tag 时触发，矩阵为 mac-arm64 / mac-x64 / win-x64 /
linux-x64，产出与上游 v0.1.60 同类：`.dmg`、`.app.tar.gz(+.sig)`、`*_x64-setup.exe(+.sig)`、
`.deb` / `.AppImage(+.sig)`，外加 `latest.json` 与各平台 `latest-*.json` 更新清单。

**推 tag 之前先在仓库里配好下面这些，否则构建会直接失败：**

| 类型 | 名称 | 用途 | 不配会怎样 |
| --- | --- | --- | --- |
| Secret | `TAURI_SIGNING_PRIVATE_KEY` | 更新包签名私钥 | 配置里 `createUpdaterArtifacts: true` 却拿不到私钥 → 桌面端构建报错退出 |
| Secret | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 私钥口令 | 同上 |
| Variable | `PUBLISH_NPM` | **二改版默认关闭** | 不设即跳过 npm，只发 GitHub Release |
| Variable | `PUBLISH_APP` | 设 `false` 可只发 npm 不发桌面 App | 不设即发桌面 App |

配法（私钥内容**不要**贴进任何聊天、issue 或提交）：

```bash
cat ~/.wb-switch-plus/wb-switch-updater.key     # 整份内容就是该 Secret 的值
```

GitHub 仓库 → Settings → Secrets and variables → Actions → Secrets 新建两项：
`TAURI_SIGNING_PRIVATE_KEY`（粘贴上面 `cat` 的内容）、
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（值 `wb-switch-dev`，与 `package.json` 里构建脚本用的
口令一致）。私钥文件本身在 `$HOME` 下、不进仓库，`package.json` 里写死的只是口令。

### 本机手动发版

1. `npm run build:app:release` 生成 `.app.tar.gz` + `.sig`（Windows NSIS 构建会额外生成当前版本的 `*_x64-setup.exe` + `.exe.sig`）。CI 会先清掉 `target/**/release/bundle`，避免 cargo cache 把旧安装包带进 Release。
2. **`npm run verify:release` 做产物自检**（见下），有命中就先修再发布。
3. macOS：`UPDATE_OS=macos UPDATE_ARCH=aarch64 sh scripts/gen-update-json.sh` 生成 `latest-macos-aarch64.json`；Intel 用 `UPDATE_ARCH=x86_64`
4. Windows：`UPDATE_OS=windows UPDATE_ARCH=x86_64 sh scripts/gen-update-json.sh` 生成 `latest-windows-x86_64.json`
5. `python3 scripts/merge-update-manifests.py <产物目录>` 合并为 `latest.json`，并把 Windows 平台项写入 `latest-macos-x86_64.json`（兼容已安装的 Windows 客户端）
6. 将安装包、签名更新包、`latest*.json` 一并上传到 GitHub Release

### 产物自检：别把构建机的路径打进二进制

`env!("CARGO_MANIFEST_DIR")` 是**编译期**常量，值是打包这台机器上的绝对路径。
把它用在 `include_bytes!` 里是安全的（编译期就把文件读进去了，路径不进二进制）；
但一旦用在运行时的 `PathBuf::from(...)` 兜底上，整条路径就会被写进可执行文件。
后果是：开发机上一切正常（路径正好存在），到了用户机器上，「找不到资源」的兜底
会静默指向开发者的源码树——出现「装的是 C:\，跑的资源却来自某个开发目录」，
而且在用户机器上完全查不出原因。

所以打包后必须扫一遍产物：

```bash
npm run verify:release                      # 默认查桌面端 exe 与反代 exe
node scripts/verify-release-binary.mjs <其它产物路径>
```

有命中就说明有人漏加了 `#[cfg(debug_assertions)]` 门闩——把那条兜底用
`#[cfg(debug_assertions)]` 包起来（只让开发态生效），再重新打包。

### 反代服务随包分发

反代服务是**独立进程**，桌面端按「用户指定 → 资源目录 → 桌面端 exe 同级 → 源码树（仅开发态）」
的顺序找它。打包时用 `src-tauri/tauri.windows.conf.json` 把它放进安装目录根：

```json
{ "bundle": { "resources": { "../reverse-proxy/dist/wb-reverse-proxy.exe": "wb-reverse-proxy.exe" } } }
```

三件事一起看，缺一件就会构建失败：

1. **只在 Windows 声明**。`wb-reverse-proxy.exe` 是 PyInstaller 打的 Windows 单文件
   （见 `reverse-proxy/build-exe.py`），在 macOS/Linux 上既不存在也没意义。基线
   `tauri.conf.json` 里**不能**出现这条 `resources`——Tauri 对 `resources` 里声明过的路径
   是硬校验，文件不存在就直接报错退出，四个平台会一起红。平台差异走
   `tauri.windows.conf.json`（官方支持的平台配置，按 JSON Merge Patch 合并，对象逐键合并）。
2. **CI 里必须先在 Windows 上构建它**。`reverse-proxy/dist/` 被 `.gitignore` 排除，
   runner 上不会有现成文件，所以 `build.yml` 在「Build desktop app」之前插了一步
   `Build reverse-proxy service`（`pip install -r reverse-proxy/requirements.txt pyinstaller`
   → `python build-exe.py`）。
3. **本机打包前也要手动跑一次** `cd reverse-proxy && python build-exe.py`，否则装进安装包的是旧 exe。
   改完 `reverse-proxy/**.py` 一定要重跑——exe 不会跟着源码自动更新。

macOS / Linux 的安装包里**没有**反代服务：`reverse_proxy.rs` 已按平台找 `wb-reverse-proxy.exe`
与 `wb-reverse-proxy` 两个名字，要在那两个平台也带上，需要把它们各自的 PyInstaller 产物声明进
`tauri.macos.conf.json` / `tauri.linux.conf.json`（`build-exe.py` 现在把输出名写死成 `.exe`，
得先改成按平台判断）。

### 改名时容易漏的地方

`tauri.conf.json` 的 `productName` 决定的不只是标题，还有装出来的**进程映像名**。二改把它
改成 `WB-Switch-Plus` 后，下面几处必须同步，否则表现为「功能静默失效」而不是报错：

- `crates/wb-switch-core/src/modules/process.rs` 的 `SELF_IMAGE_STEMS`：这份清单是**精确匹配**，
  漏掉新名字会认不出自身进程——「关闭 CodeBuddy IDE」时会把自己的进程也算进待关闭列表。
  同时保留 `workbuddy-switch` / `wb-switch`，让从上游版本升级上来的用户仍能被识别。
- `scripts/make-dmg.sh` 的 `.app` 路径与 dmg 文件名、`scripts/fix-app.sh` 的 `.app` 路径。
- `.github/workflows/build.yml` 里 Tauri 直接产出的文件名按 productName 生成
  （`WB-Switch-Plus_${VER}_x64-setup.exe`、`WB-Switch-Plus.app`），而**发布到 Release 的资产名**
  用的是 `wb-switch-plus-*` 前缀——两者不同，重命名的那一步别搞混。

### npm 版（webui）发布

> 已移除：浏览器 webui 形态随 `crates/wb-switch-server` 一起删除，npm 包与平台二进制不再发布。
> 现在只发桌面 App（Release 资产 + updater）。

## 目录结构

```
src-tauri/
  src/
    commands.rs      # Tauri command 薄包装
    proxy_service.rs # 内置反代的生命周期（autostart / 停机 / 端口与监听范围 / 进程内管理面通道）
    modules/         # 已抽离到 crates/wb-switch-core
crates/
  wb-switch-core/    # 核心逻辑：account/auth_file/oauth/process/switch/session/checkin/refresh/update/config
  wb-switch-proxy/   # 内置反代：数据面是唯一 HTTP 面（/health + /v1/*）；管理面进程内调用
src/                 # 前端：components/pages/lib（api.ts 只走 Tauri invoke；反代管理面走 reverse-proxy.ts 的进程内通道）
reverse-proxy/       # Python 反代（反代服务old，回滚通道）；dist/wb-reverse-proxy.exe 随安装包分发
scripts/             # 构建与发布辅助脚本（含 verify-release-binary.mjs 产物自检）
```

## 隐私注意事项

- 仓库不提交本地数据（accounts.json、认证文件、密钥、token 由 `.gitignore` 排除）
- 发布前用 `git grep` 扫描 token 模式（`ghp_`/`npm_`/`gho_` 等）
