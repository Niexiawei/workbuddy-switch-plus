#!/usr/bin/env bash
# Windows x64 一键构建：反代服务 exe + 前端 + 桌面端 NSIS 安装包
#
# 用法（仓库根目录，Git Bash）：
#   sh scripts/build-win-x64.sh            # 完整构建
#   sh scripts/build-win-x64.sh --check    # 只做前置自检，不构建
#
# 可选环境变量：
#   WB_PYTHON=/path/to/python.exe   指定打包反代服务用的 Python（默认自动找 python/python3）
#   SKIP_DEPS=1                     跳过 npm install
#   SKIP_REVERSE_PROXY=1            跳过反代 exe（调试时用；**正常构建不要加**，
#                                   因为 tauri.windows.conf.json 把它声明成必需资源，
#                                   跳过且文件不存在时 tauri build 会直接报错退出）
#
# 说明：本脚本只覆盖 Windows。macOS / Linux 请直接 `npx tauri build`，
# 本项目未在那两个平台验证过（见 README）。
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1
ROOT=$(pwd -P)
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

say()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
info() { printf '    %s\n' "$*"; }
warn() { printf '\033[1;33m    [注意] %s\033[0m\n' "$*"; }
die()  { printf '\n\033[1;31m[错误] %s\033[0m\n' "$*" >&2; exit 1; }
ok()   { printf '\033[1;32m    OK  %s\033[0m\n' "$*"; }

# 等某个刚写出的文件不再被独占。
#
# 为什么需要：PyInstaller 刚生成的大 exe 经常被 Windows Defender / 索引器短暂攥住，
# 而紧接着的 tauri build 里，cargo 构建脚本要读它算 `rerun-if-changed` 的哈希，
# 撞上就是以
#   另一个程序正在使用此文件，进程无法访问。(os error 32)
# 整场构建失败。文件过一会儿自己会释放，重跑十几分钟纯属浪费。
#
# 用只读探测：cargo 只需要读，只读都打不开才说明真的挡住了它。
wait_released() {
  _path="$1"; _tries="${2:-30}"; _i=0
  # 先沉降一下再探：杀软的扫描有时是文件落地之后才开始的，立刻探会误判成「已释放」
  sleep 2
  while [ "$_i" -lt "$_tries" ]; do
    if "$PY" -c 'import sys; open(sys.argv[1], "rb").close()' "$_path" 2>/dev/null; then
      [ "$_i" -gt 0 ] && ok "已释放（等了 $((_i + 2))s）：$(basename "$_path")"
      return 0
    fi
    [ "$_i" -eq 0 ] && info "文件被独占（通常是杀软在扫描），等它释放…"
    _i=$((_i + 1)); sleep 1
  done
  warn "$(basename "$_path") 等了 ${_tries}s 仍读不到"
  return 0
}

# ---------------------------------------------------------------- 前置自检

say "前置自检"

case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ok "平台：Windows ($(uname -s))" ;;
  *) die "本脚本只针对 Windows x64（当前 $(uname -s)）。macOS/Linux 请直接 npx tauri build。" ;;
esac

command -v node >/dev/null 2>&1 || die "找不到 node。请安装 Node 20+：https://nodejs.org"

# node 能启动 ≠ 能执行 JS。踩过一种很难查的坏法：`node -v` 照常输出，
# 但任何 `-p`/`-e`/`--check` 都以 ACCESS_VIOLATION 崩掉（Git Bash 里 rc=139，
# 即 0xC0000005），stdout、stderr **全空**。配上 set -e，整个脚本就
# 「一个字不说直接退出」，看起来像卡在上一行，查起来极费劲。
# 所以先让它真跑一段 JS，跑不了就带诊断信息退出。
NODE_BIN=$(command -v node)
NODE_MAJOR=$(node -p "process.versions.node.split('.')[0]" 2>/dev/null) || NODE_MAJOR=""
case "$NODE_MAJOR" in
  ''|*[!0-9]*)
    die "node 无法执行 JS：$NODE_BIN
     先自己复现一下：   node -p \"1+1\"
     若退出码非 0 且没有任何输出（Windows 上常见 139 / 0xC0000005），
     说明这个 node 在当前位置起不来 —— 多为 node 安装损坏，或安全软件
     对 Program Files 下可执行文件的注入/拦截。
     换一个能跑的 node 再重跑：
       export PATH=\"/path/to/usable/node-dir:\$PATH\"
       node -p \"1+1\"     # 能输出 2 就对了
     注意 npm / npx 同样是靠 node 跑脚本的，PATH 里必须换成好的那个。" ;;
esac
[ "$NODE_MAJOR" -ge 20 ] || die "需要 Node 20 或更高，当前 $(node -v)"
ok "Node $(node -v)"

# 产品名与版本：第 5 步靠它精确匹配安装包名（别用字典序找，见第 5 步的注释）。
# 放在 node 自检之后，读不到就直接报错，而不是像以前那样被 2>/dev/null 吞成空字符串。
TAURI_CONF="src-tauri/tauri.conf.json"
[ -f "$TAURI_CONF" ] || die "找不到 $TAURI_CONF（请在仓库根目录运行本脚本）"
PRODUCT_NAME=$(node -e "process.stdout.write(require(process.cwd()+'/$TAURI_CONF').productName||'')") \
  || die "读取 $TAURI_CONF 的 productName 失败"
APP_VERSION=$(node -e "process.stdout.write(String(require(process.cwd()+'/$TAURI_CONF').version||''))") \
  || die "读取 $TAURI_CONF 的 version 失败"
if [ -n "$PRODUCT_NAME" ]; then
  ok "产品名：$PRODUCT_NAME   版本：${APP_VERSION:-未知}"
else
  warn "$TAURI_CONF 里没有 productName，第 5 步只能退回「找最新的安装包」"
fi

# rustup 的 shim 在部分 Windows 安装上是坏的：`rustc -vV` 无输出但退出码为 0，
# cargo 会因此静默不编译（不报错、直接跳过），表现为"构建成功但产物没变"。
# 所以这里不能只看退出码，要确认真的有输出；没有就绕过 shim 直接用 toolchain 真身。
if [ -z "$(rustc -vV 2>/dev/null | head -1 || true)" ]; then
  TOOLCHAIN_BIN=""
  for d in "$HOME"/.rustup/toolchains/*/bin; do
    if [ -x "$d/rustc.exe" ] || [ -x "$d/rustc" ]; then TOOLCHAIN_BIN="$d"; break; fi
  done
  [ -n "$TOOLCHAIN_BIN" ] || die "找不到可用的 rustc。请安装 Rust（https://rustup.rs）并选 MSVC 工具链。"
  export PATH="$TOOLCHAIN_BIN:$PATH"
  warn "rustup shim 无输出（已损坏），改用 toolchain 真身：$TOOLCHAIN_BIN"
fi
rustc --version >/dev/null || die "rustc 不可用"
ok "$(rustc --version)"

PY="${WB_PYTHON:-}"
# 优先用仓库内的 venv（如果建过），避免往系统 Python 里装 PyInstaller
if [ -z "$PY" ]; then
  for c in .venv/Scripts/python.exe .venv/bin/python \
           reverse-proxy/.venv/Scripts/python.exe reverse-proxy/.venv/bin/python; do
    if [ -x "$c" ]; then PY="$c"; break; fi
  done
fi
if [ -z "$PY" ]; then
  for c in python python3; do
    if command -v "$c" >/dev/null 2>&1; then PY="$c"; break; fi
  done
fi
[ -n "$PY" ] || die "找不到 Python（打包反代服务需要）。用 WB_PYTHON=/path/to/python 指定。"
ok "$($PY --version 2>&1)  ($(command -v "$PY"))"

# 光找到 Python 不够：打反代 exe 要 PyInstaller，运行/打包还要 fastapi+uvicorn+httpx。
# 挑到没装这些包的解释器时，会拖到第 2 步才炸，报错还很难懂——这里提前判掉。
MISSING_PKGS=$("$PY" -c "import importlib.util as u; print(' '.join(n for n in ('fastapi','uvicorn','httpx','PyInstaller') if u.find_spec(n) is None))" 2>/dev/null || echo "<探测失败>")
if [ -n "$MISSING_PKGS" ]; then
  die "选中的 Python 缺少打包反代服务需要的包：$MISSING_PKGS
     换个装好依赖的解释器重跑：
       WB_PYTHON=/path/to/python sh scripts/build-win-x64.sh
     或者仓库根目录先建一个 venv：
       python -m venv .venv
       .venv/Scripts/python -m pip install -r reverse-proxy/requirements.txt pyinstaller
     （建好后本脚本会自动优先用它）"
fi
ok "反代打包依赖齐备（fastapi / uvicorn / httpx / PyInstaller）"

# 静态资源语法闸门。
#
# 为什么必须有：`admin/static/app.js` 曾被截断（`setAccountMode` 开了没关，
# 后面的代码全被吞进它的函数体），导致整个 /admin/ 控制台**静默白屏** ——
# 而它**不会让任何后端测试变红**，pytest 全绿、界面全白。
#
# 这种损坏之所以能活到构建阶段，是因为「谁改了静态文件、有没有语法错」
# 此前没有任何机制保证，只靠人记得跑 node --check。**自觉不是机制。**
# 这里把它变成硬门：任何静态 JS 语法不过，构建直接失败。
# 它不依赖任何人记得做 —— 而是「不做就构建不出来」。
node --version >/dev/null 2>&1 || die "静态资源语法检查需要 node"
STATIC_DIR="reverse-proxy/admin/static"
if [ -d "$STATIC_DIR" ]; then
  STATIC_BAD=""
  for js in "$STATIC_DIR"/*.js; do
    [ -e "$js" ] || continue
    if node --check "$js" >/dev/null 2>&1; then
      ok "语法通过：$(basename "$js") ($(wc -l < "$js" | tr -d " ") 行)"
    else
      warn "语法错误：$js"
      node --check "$js" 2>&1 | sed "s/^/      /" || true
      STATIC_BAD="$STATIC_BAD $(basename "$js")"
    fi
  done
  [ -z "$STATIC_BAD" ] || die "静态资源存在语法错误，构建中止：$STATIC_BAD
     这类文件语法坏掉会让整个页面白屏，且不会让任何后端测试变红 ——
     所以必须在构建前拦住。本地先跑：node --check <文件> 修完再构建。"
else
  warn "未找到 $STATIC_DIR，跳过静态资源语法检查"
fi

# 更新签名私钥：tauri.conf.json 里 createUpdaterArtifacts=true，
# 拿不到私钥时 tauri build 会直接失败（不是跳过签名）。
KEY="$HOME/.wb-switch-plus/wb-switch-updater.key"
if [ -f "$KEY" ]; then
  ok "更新签名私钥：$KEY"
else
  die "缺少更新签名私钥 $KEY
     配置里 createUpdaterArtifacts=true，没有它 tauri build 会直接报错。
     生成一把：
       npx tauri signer generate -p <口令> -w \"$KEY\" --ci
     然后同步更新 src-tauri/tauri.conf.json 的 plugins.updater.pubkey（同目录 .pub 的整份内容）。"
fi

if [ "$CHECK_ONLY" = "1" ]; then
  say "--check 模式：前置自检通过，未执行构建"
  exit 0
fi

# ---------------------------------------------------------------- 1 依赖

if [ "${SKIP_DEPS:-0}" = "1" ]; then
  say "1/5 安装前端依赖（SKIP_DEPS=1，跳过）"
else
  say "1/5 安装前端依赖"
  # 刻意用 install 而不是 ci：ci 会先清空 node_modules，
  # 那一步在本机容易被文件批量删除保护拦下而中断。
  npm install --no-audit --no-fund
fi

# ---------------------------------------------------------------- 2 反代 exe

say "2/5 打包反代服务（单文件 exe）"
if [ "${SKIP_REVERSE_PROXY:-0}" = "1" ]; then
  warn "SKIP_REVERSE_PROXY=1，跳过反代打包；下面 tauri build 会因为缺资源而失败"
else
  if ! "$PY" -c "import PyInstaller" >/dev/null 2>&1; then
    info "安装 PyInstaller 与反代依赖…"
    "$PY" -m pip install --disable-pip-version-check -q \
      -r reverse-proxy/requirements.txt pyinstaller
  fi
  ( cd reverse-proxy && "$PY" build-exe.py )
  RP_EXE="reverse-proxy/dist/wb-reverse-proxy.exe"
  [ -f "$RP_EXE" ] || die "反代 exe 未生成：$RP_EXE"
  ok "$RP_EXE ($(du -h "$RP_EXE" | cut -f1))"
  # 必须等它被独占结束（第 4 步的 cargo 构建脚本要读它算 rerun-if-changed 哈希）
  wait_released "$RP_EXE"
  info "它会被 src-tauri/tauri.windows.conf.json 声明进安装目录根"
fi

# ---------------------------------------------------------------- 3 让出 dist

say "3/5 准备前端产物目录"
if [ -d dist ]; then
  OLD="dist.old-$(date +%s)"
  mv dist "$OLD"
  info "旧 dist 已挪到 $OLD（不删除，避开批量删除保护；确认构建正常后可自行清理）"
else
  info "没有旧 dist，跳过"
fi

# ---------------------------------------------------------------- 4 tauri build

say "4/5 构建桌面端（NSIS 安装包）"
export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-wb-switch-dev}"
info "beforeBuildCommand 会自动跑 npm run build:desktop（前端 + Companion + agent-studio-runtime）"
# 反代 exe 的 mtime 要比源码新，否则装进包里的是旧版本——这里顺带提醒
if [ -f reverse-proxy/dist/wb-reverse-proxy.exe ]; then
  NEWEST_PY=$(find reverse-proxy -name '*.py' -newer reverse-proxy/dist/wb-reverse-proxy.exe 2>/dev/null | head -3 || true)
  [ -n "$NEWEST_PY" ] && warn "反代源码比 exe 新，建议重跑第 2 步：$NEWEST_PY"
fi
npx tauri build --bundles nsis

# ---------------------------------------------------------------- 5 汇总

say "5/5 完成"

INSTALLER=""
BUNDLE_DIR=""
for d in src-tauri/target/release/bundle/nsis target/release/bundle/nsis; do
  [ -d "$d" ] && BUNDLE_DIR="$d" && break
done
[ -n "$BUNDLE_DIR" ] || die "构建结束但没找到 NSIS 产物目录，请检查上面的日志"

# 按「产品名_版本」精确找，别用 `find | sort | tail -1`：那是**字典序**，
# `WB-Switch-Plus_0.1.0_` 排在 `WB-Switch-Plus_0.1.60_` 前面，会把上一次构建的旧包报成新产物。
EXPECTED="$BUNDLE_DIR/${PRODUCT_NAME}_${APP_VERSION}_x64-setup.exe"
if [ -n "$PRODUCT_NAME" ] && [ -n "$APP_VERSION" ] && [ -f "$EXPECTED" ]; then
  INSTALLER="$EXPECTED"
else
  # 名字对不上（改了 productName、或平台后缀不是 x64）时退回「最新的那个」
  INSTALLER=$(ls -t "$BUNDLE_DIR"/*-setup.exe 2>/dev/null | head -1 || true)
  [ -n "$INSTALLER" ] && [ ! -f "$EXPECTED" ] && warn "没找到预期文件 ${PRODUCT_NAME}_${APP_VERSION}_x64-setup.exe，改用最新的 $INSTALLER"
fi
[ -n "$INSTALLER" ] || die "构建结束但没找到 NSIS 安装包，请检查上面的日志"

# 同一个目录里可能还躺着历史版本的安装包，明确提醒一下，免得拿错文件去发布
STALE=$(ls "$BUNDLE_DIR"/*-setup.exe 2>/dev/null | grep -Fv -- "$INSTALLER" || true)
[ -n "$STALE" ] && warn "同目录还有历史安装包（发布时别拿错）：$(printf '%s ' $STALE)"

printf '    安装包      : %s (%s)\n' "$INSTALLER" "$(du -h "$INSTALLER" | cut -f1)"
[ -f "$INSTALLER.sig" ] && printf '    更新签名    : %s\n' "$INSTALLER.sig"

APP_EXE=""
for d in target/release src-tauri/target/release; do
  [ -f "$d/wb-switch-rust.exe" ] && APP_EXE="$d/wb-switch-rust.exe" && break
done
[ -n "$APP_EXE" ] && printf '    裸可执行文件: %s\n' "$APP_EXE"

printf '\n    发布前请跑一次产物自检：  npm run verify:release\n'
