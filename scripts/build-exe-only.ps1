#Requires -Version 5.1
<#
.SYNOPSIS
    只构建桌面端单可执行文件（target\release\wb-switch-rust.exe），不打安装包、不打包反代 exe。

.DESCRIPTION
    与 build-win-x64.ps1 的区别：
      · 不跑 PyInstaller（不产出 wb-reverse-proxy.exe）；
      · 不跑 makensis（不产出 NSIS 安装包）；
      · 只做：前端产物（vite build）→ cargo build --release → 校验无构建机路径泄漏。

    产物：target\release\wb-switch-rust.exe（内嵌 dist/ 前端与 Companion 资源）。
    用途：本机快速验证 / 拷贝到其它机器直接运行（绿色形态，无需安装）。

    注意：这个 exe 是**桌面端宿主**，内置反代随它启动（数据面 /health + /v1/*）。
    它不包含「反代服务old」的 Python exe——那需要完整构建（build-win-x64.ps1）。

.EXAMPLE
    .\scripts\build-exe-only.ps1                # 正常构建（前端 + Rust）
    .\scripts\build-exe-only.ps1 -SkipFrontend  # 前端 dist 已是最新时，只重编 Rust
    .\scripts\build-exe-only.ps1 -SkipDeps      # 跳过 npm install

.NOTES
    若提示「禁止运行脚本」：powershell -ExecutionPolicy Bypass -File scripts\build-exe-only.ps1
#>

[CmdletBinding()]
param(
    # 跳过前端构建（dist/ 已是最新时用；Rust 的 include/嵌入在 tauri build 时读 dist）
    [switch]$SkipFrontend,
    # 跳过 npm install（依赖没动过时用）
    [switch]$SkipDeps
)

$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

Set-Location (Split-Path -Parent $PSScriptRoot)

function Say  { param([string]$Msg) Write-Host ''; Write-Host "==> $Msg" -ForegroundColor Cyan }
function Info { param([string]$Msg) Write-Host "    $Msg" }
function Ok   { param([string]$Msg) Write-Host "    OK  $Msg" -ForegroundColor Green }
function Fail {
    param([string]$Msg)
    Write-Host ''
    Write-Host "[错误] $Msg" -ForegroundColor Red
    exit 1
}
function Assert-LastExit { param([string]$Msg) if ($LASTEXITCODE -ne 0) { Fail $Msg } }

Say '前置自检'
if (-not (Get-Command node -ErrorAction SilentlyContinue)) { Fail '未找到 node' }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail '未找到 cargo（Rust 工具链）' }
$version = (Get-Content package.json -Raw | ConvertFrom-Json).version
Ok ("产品名：WB-Switch-Plus   版本：{0}" -f $version)

# ---------------------------------------------------------------- 依赖

if (-not $SkipDeps) {
    Say '1/3 安装前端依赖'
    npm install --no-fund --no-audit
    Assert-LastExit 'npm install 失败'
} else {
    Info '跳过 npm install（-SkipDeps）'
}

# ---------------------------------------------------------------- 前端

if (-not $SkipFrontend) {
    Say '2/3 构建前端（vite build → dist/，含 Companion embed 资源）'
    # 与 tauri beforeBuildCommand 同口径：build:desktop = companion build + vite build + companion copy
    npm run build:desktop
    Assert-LastExit '前端构建失败'
    if (-not (Test-Path 'dist/index.html')) { Fail 'dist/index.html 不存在，前端产物异常' }
    Ok 'dist/ 就绪'
} else {
    Info '跳过前端构建（-SkipFrontend）；沿用现有 dist/'
    if (-not (Test-Path 'dist/index.html')) {
        Fail 'dist/index.html 不存在——当前没有前端产物，请去掉 -SkipFrontend'
    }
}

# ---------------------------------------------------------------- Rust release

Say '3/3 cargo build --release（桌面端单文件）'
# 用 tauri build 的 feature 口径（custom-protocol 决定资源加载形态），
# 但只编译不打包：直接调 cargo 而不是 tauri build --no-bundle——少一层插件检查。
cargo build --release -p wb-switch-rust --features custom-protocol
Assert-LastExit 'cargo build 失败'

$exe = 'target\release\wb-switch-rust.exe'
if (-not (Test-Path $exe)) { Fail "产物不存在：$exe" }

Say '产物校验'
node scripts/verify-release-binary.mjs
Assert-LastExit '产物校验失败（存在构建机路径泄漏？）'

$size = (Get-Item $exe).Length / 1MB
Write-Host ''
Write-Host "完成：$exe  ($('{0:N1}' -f $size) MB)" -ForegroundColor Green
Write-Host '拷到其它机器可直接运行（绿色形态）。内置反代随启动生效（数据面 /health + /v1/*）。'
Write-Host '需要 NSIS 安装包 / 反代old exe 时，改跑 scripts\build-win-x64.ps1。'
