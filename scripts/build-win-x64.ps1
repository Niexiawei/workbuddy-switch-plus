#Requires -Version 5.1
<#
.SYNOPSIS
    Windows x64 一键构建（PowerShell 版）：反代服务 exe + 前端 + 桌面端 NSIS 安装包

.DESCRIPTION
    与 scripts/build-win-x64.sh 步骤一一对应，只是换成 PowerShell 实现，
    方便不装 Git Bash 的机器直接用（也顺手绕开 Git Bash 的路径/引号坑）。

.EXAMPLE
    .\scripts\build-win-x64.ps1                    # 完整构建
    .\scripts\build-win-x64.ps1 -CheckOnly         # 只做前置自检，不构建
    .\scripts\build-win-x64.ps1 --check            # 同上（兼容 sh 版写法）

.NOTES
    可选环境变量（与 sh 版一致）：
      WB_PYTHON=/path/to/python.exe   指定打包反代服务用的 Python（默认自动找 python/python3）
      SKIP_DEPS=1                     跳过 npm install
      SKIP_REVERSE_PROXY=1            跳过反代 exe（调试时用；正常构建不要加，
                                      因为 tauri.windows.conf.json 把它声明成必需资源，
                                      跳过且文件不存在时 tauri build 会直接报错退出）

    若提示「禁止运行脚本 / 无法加载文件」，用这条启动：
      powershell -ExecutionPolicy Bypass -File scripts\build-win-x64.ps1
#>

[CmdletBinding()]
param(
    [switch]$CheckOnly,
    # 兼容 sh 版的 `--check`：PowerShell 会把它当普通字符串传进来
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Rest
)

# 步骤里全是外部命令，这里只让 cmdlet 的失败变成终止错误；外部命令的退出码
# 统一由 Assert-LastExit 检查（PowerShell 本身不会因退出码非 0 而停下）。
$ErrorActionPreference = 'Stop'
# 控制台输出统一 UTF-8，避免中文在 GBK 控制台里变乱码；
# 输出被重定向（无控制台句柄）时这里会抛，忽略即可。
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

$CheckFlag = '-CheckOnly'
foreach ($a in $Rest) {
    if ($a -match '^-{1,2}check$') { $CheckOnly = $true; $CheckFlag = $a }
}

# 切到仓库根目录（脚本在 scripts/ 下），后续所有相对路径都以根为基准
Set-Location (Split-Path -Parent $PSScriptRoot)

# ---------------------------------------------------------------- 输出helper

function Say  { param([string]$Msg) Write-Host ''; Write-Host "==> $Msg" -ForegroundColor Cyan }
function Info { param([string]$Msg) Write-Host "    $Msg" }
function Warn { param([string]$Msg) Write-Host "    [注意] $Msg" -ForegroundColor Yellow }
function Ok   { param([string]$Msg) Write-Host "    OK  $Msg" -ForegroundColor Green }
function Fail {
    param([string]$Msg)
    Write-Host ''
    Write-Host "[错误] $Msg" -ForegroundColor Red
    exit 1
}

# 外部命令返回非 0 就停（等价于 sh 版的 set -e 对原生命令的效果）
function Assert-LastExit {
    param([string]$What)
    if ($LASTEXITCODE -ne 0) {
        Fail "$What 失败（退出码 $LASTEXITCODE）"
    }
}

# 跑外部命令并取 stdout，结果存 $script:LastNativeCode。
#
# 为什么不直接 $(...)：
#   1) PowerShell 5.1 在 $ErrorActionPreference='Stop' 时，外部命令往 stderr
#      写一行就会被当成终止错误（尤其配 2>&1），构建脚本里一堆工具都会
#      正常输出警告到 stderr，会无端中断。这里临时放宽成 Continue。
#   2) 顺手把 stdout 收敛成字符串，免得 @() 数组和单值两种形态到处判断。
function Invoke-Capture {
    param(
        [string]$Exe,
        [string[]]$Arguments,
        [switch]$MergeStderr
    )
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        if ($MergeStderr) { $out = & $Exe @Arguments 2>&1 } else { $out = & $Exe @Arguments 2>$null }
        $script:LastNativeCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($null -eq $out) { return '' }
    if ($out -is [array]) { return ($out -join "`n") }
    return [string]$out
}

# Windows 上 npm / npx 各有三份同名前缀：npm.cmd、npm.ps1、npm。
# PowerShell 会优先命中 .ps1，执行策略受限时直接报「禁止运行脚本」。
# 固定走 .cmd 更稳（构建本身不依赖 PowerShell 的脚本执行权限）。
function Get-ToolExe {
    param([string]$Name)
    foreach ($n in @("$Name.cmd", "$Name.exe")) {
        $c = Get-Command $n -ErrorAction SilentlyContinue
        if ($c) { return $c.Source }
    }
    $c = Get-Command $Name -ErrorAction SilentlyContinue
    if ($c) { return $c.Source }
    return $null
}

# 等某个刚写出的文件不再被独占。
#
# 为什么需要：PyInstaller 刚生成的大 exe 经常被 Windows Defender / 索引器短暂攥住，
# 而紧接着的 tauri build 里，cargo 构建脚本要读它算 rerun-if-changed 的哈希，
# 撞上就是以「另一个程序正在使用此文件，进程无法访问 (os error 32)」整场失败。
# 文件过一会儿自己会释放，重跑十几分钟纯属浪费。
#
# 用只读探测：cargo 只需要读，只读都打不开才说明真的挡住了它。
function Wait-Released {
    param([string]$Path, [int]$Tries = 30)
    # 先沉降一下再探：杀软的扫描有时是文件落地之后才开始的，立刻探会误判成「已释放」
    Start-Sleep -Seconds 2
    for ($i = 0; $i -lt $Tries; $i++) {
        $opened = $false
        try {
            $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
            $fs.Close()
            $opened = $true
        } catch { }
        if ($opened) {
            if ($i -gt 0) { Ok "已释放（等了 $($i + 2)s）：$(Split-Path -Leaf $Path)" }
            return
        }
        if ($i -eq 0) { Info '文件被独占（通常是杀软在扫描），等它释放…' }
        Start-Sleep -Seconds 1
    }
    Warn "$(Split-Path -Leaf $Path) 等了 ${Tries}s 仍读不到"
}

function Get-SizeMB {
    param([string]$Path)
    return ('{0:N1} MB' -f ((Get-Item $Path).Length / 1MB))
}

# ---------------------------------------------------------------- 前置自检

Say '前置自检'

if ($env:OS -ne 'Windows_NT') {
    Fail "本脚本只针对 Windows x64（当前 $([System.Environment]::OSVersion.VersionString)）。macOS/Linux 请直接 npx tauri build。"
}
if (-not [System.Environment]::Is64BitOperatingSystem) {
    Fail '需要 64 位 Windows'
}
Ok "平台：Windows $([System.Environment]::OSVersion.VersionString)（$env:PROCESSOR_ARCHITECTURE）"

$nodeCmd = Get-Command node -ErrorAction SilentlyContinue
if (-not $nodeCmd) { Fail '找不到 node。请安装 Node 20+：https://nodejs.org' }

# node 能启动 ≠ 能执行 JS。踩过一种很难查的坏法：`node -v` 照常输出，
# 但任何 `-p`/`-e`/`--check` 都以 ACCESS_VIOLATION 崩掉（Git Bash 里 rc=139，
# 即 0xC0000005），stdout、stderr 全空。配上「退出码即中断」的处理方式，
# 整个脚本会一个字不说直接退出，看起来像卡在上一行，查起来极费劲。
# 所以先让它真跑一段 JS，跑不了就带诊断信息退出。
$NodeBin = $nodeCmd.Source
$NodeMajor = (Invoke-Capture 'node' @('-p', "process.versions.node.split('.')[0]")).Trim()
if ($script:LastNativeCode -ne 0 -or $NodeMajor -notmatch '^\d+$') {
    Fail @"
node 无法执行 JS：$NodeBin
     先自己复现一下：   node -p "1+1"
     若退出码非 0 且没有任何输出（Windows 上常见 139 / 0xC0000005），
     说明这个 node 在当前位置起不来 —— 多为 node 安装损坏，或安全软件
     对 Program Files 下可执行文件的注入/拦截。
     换一个能跑的 node 再重跑（把它的目录放到 PATH 最前面）：
       `$env:PATH = '<可用的 node 目录>;' + `$env:PATH
       node -p "1+1"     # 能输出 2 就对了
     注意 npm / npx 同样是靠 node 跑脚本的，PATH 里必须换成好的那个。
"@
}
if ([int]$NodeMajor -lt 20) {
    Fail "需要 Node 20 或更高，当前 $((Invoke-Capture 'node' @('-v')).Trim())"
}
Ok "Node $((Invoke-Capture 'node' @('-v')).Trim())"

# 产品名与版本：第 5 步靠它精确匹配安装包名（别用字典序找，见第 5 步的注释）。
# 直接用 ConvertFrom-Json 读，坏 JSON 会在这里就报出来，而不是变成空字符串一路带下去。
$TauriConf = 'src-tauri/tauri.conf.json'
if (-not (Test-Path $TauriConf)) { Fail "找不到 $TauriConf（请在仓库根目录运行本脚本）" }
try {
    $conf = Get-Content $TauriConf -Raw -Encoding UTF8 | ConvertFrom-Json
} catch {
    Fail "解析 $TauriConf 失败：$($_.Exception.Message)"
}
$ProductName = [string]$conf.productName
$AppVersion = [string]$conf.version
if ($ProductName) {
    Ok "产品名：$ProductName   版本：$(if ($AppVersion) { $AppVersion } else { '未知' })"
} else {
    Warn "$TauriConf 里没有 productName，第 5 步只能退回「找最新的安装包」"
}

# rustup 的 shim 在部分 Windows 安装上是坏的：`rustc -vV` 无输出但退出码为 0，
# cargo 会因此静默不编译（不报错、直接跳过），表现为"构建成功但产物没变"。
# 所以这里不能只看退出码，要确认真的有输出；没有就绕过 shim 直接用 toolchain 真身。
if (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
    Fail '找不到 rustc。请安装 Rust（https://rustup.rs）并选 MSVC 工具链。'
}
if (-not (Invoke-Capture 'rustc' @('-vV')).Trim()) {
    $ToolchainBin = $null
    $tcRoot = Join-Path $env:USERPROFILE '.rustup\toolchains'
    foreach ($d in @(Get-ChildItem $tcRoot -Directory -ErrorAction SilentlyContinue)) {
        $cand = Join-Path $d.FullName 'bin'
        if (Test-Path (Join-Path $cand 'rustc.exe')) { $ToolchainBin = $cand; break }
    }
    if (-not $ToolchainBin) {
        Fail '找不到可用的 rustc。请安装 Rust（https://rustup.rs）并选 MSVC 工具链。'
    }
    $env:PATH = "$ToolchainBin;$env:PATH"
    Warn "rustup shim 无输出（已损坏），改用 toolchain 真身：$ToolchainBin"
}
$rustcVersion = (Invoke-Capture 'rustc' @('--version')).Trim()
if (-not $rustcVersion) { Fail 'rustc 不可用' }
Ok $rustcVersion

$Py = $env:WB_PYTHON
# 优先用仓库内的 venv（如果建过），避免往系统 Python 里装 PyInstaller
if (-not $Py) {
    foreach ($c in @('.venv/Scripts/python.exe', '.venv/bin/python',
                     'reverse-proxy/.venv/Scripts/python.exe', 'reverse-proxy/.venv/bin/python')) {
        if (Test-Path $c) { $Py = (Resolve-Path $c).Path; break }
    }
}
if (-not $Py) {
    foreach ($c in @('python', 'python3')) {
        $found = Get-Command $c -ErrorAction SilentlyContinue
        if ($found) { $Py = $found.Source; break }
    }
}
if (-not $Py) {
    Fail '找不到 Python（打包反代服务需要）。用 WB_PYTHON=/path/to/python 指定。'
}
Ok "$((Invoke-Capture $Py @('--version')).Trim())  ($Py)"

# 光找到 Python 不够：打反代 exe 要 PyInstaller，运行/打包还要 fastapi+uvicorn+httpx。
# 挑到没装这些包的解释器时，会拖到第 2 步才炸，报错还很难懂——这里提前判掉。
$probe = @'
import importlib.util as u
print(' '.join(n for n in ('fastapi', 'uvicorn', 'httpx', 'PyInstaller') if u.find_spec(n) is None))
'@
$missingPkgs = (Invoke-Capture $Py @('-c', $probe)).Trim()
if ($script:LastNativeCode -ne 0) {
    Fail "用 $Py 探测依赖失败（退出码 $($script:LastNativeCode)），换个解释器试试：WB_PYTHON=/path/to/python"
}
if ($missingPkgs) {
    Fail @"
选中的 Python 缺少打包反代服务需要的包：$missingPkgs
     换个装好依赖的解释器重跑：
       `$env:WB_PYTHON = 'C:\path\to\python.exe'; .\scripts\build-win-x64.ps1
     或者仓库根目录先建一个 venv：
       python -m venv .venv
       .venv\Scripts\python -m pip install -r reverse-proxy\requirements.txt pyinstaller
     （建好后本脚本会自动优先用它）
"@
}
Ok '反代打包依赖齐备（fastapi / uvicorn / httpx / PyInstaller）'

# 静态资源语法闸门。
#
# 为什么必须有：`admin/static/app.js` 曾被截断（setAccountMode 开了没关，
# 后面的代码全被吞进它的函数体），导致整个 /admin/ 控制台静默白屏 ——
# 而它不会让任何后端测试变红，pytest 全绿、界面全白。
#
# 这种损坏之所以能活到构建阶段，是因为「谁改了静态文件、有没有语法错」
# 此前没有任何机制保证，只靠人记得跑 node --check。自觉不是机制。
# 这里把它变成硬门：任何静态 JS 语法不过，构建直接失败。
$StaticDir = 'reverse-proxy/admin/static'
if (Test-Path $StaticDir) {
    $badJs = @()
    foreach ($js in @(Get-ChildItem $StaticDir -Filter *.js -File)) {
        $detail = Invoke-Capture 'node' @('--check', $js.FullName) -MergeStderr
        if ($script:LastNativeCode -ne 0) {
            Warn "语法错误：$($js.Name)"
            foreach ($line in ($detail -split "`n")) {
                if ($line.Trim()) { Write-Host "      $line" }
            }
            $badJs += $js.Name
        } else {
            Ok "语法通过：$($js.Name) ($((Get-Content $js.FullName).Count) 行)"
        }
    }
    if ($badJs.Count -gt 0) {
        Fail @"
静态资源存在语法错误，构建中止：$($badJs -join ' ')
     这类文件语法坏掉会让整个页面白屏，且不会让任何后端测试变红 ——
     所以必须在构建前拦住。本地先跑：node --check <文件> 修完再构建。
"@
    }
} else {
    Warn "未找到 $StaticDir，跳过静态资源语法检查"
}

# 更新签名私钥：tauri.conf.json 里 createUpdaterArtifacts=true，
# 拿不到私钥时 tauri build 会直接失败（不是跳过签名）。
$Key = Join-Path $env:USERPROFILE '.wb-switch-plus\wb-switch-updater.key'
if (Test-Path $Key) {
    Ok "更新签名私钥：$Key"
} else {
    Fail @"
缺少更新签名私钥 $Key
     配置里 createUpdaterArtifacts=true，没有它 tauri build 会直接报错。
     生成一把：
       npx tauri signer generate -p <口令> -w "$Key" --ci
     然后同步更新 $TauriConf 的 plugins.updater.pubkey（同目录 .pub 的整份内容）。
"@
}

if ($CheckOnly) {
    Say "$CheckFlag 模式：前置自检通过，未执行构建"
    exit 0
}

$npmExe = Get-ToolExe 'npm'
$npxExe = Get-ToolExe 'npx'
if (-not $npmExe) { Fail '找不到 npm（node 装了但没有 npm？）。检查 PATH 里的 Node 安装是否完整。' }
if (-not $npxExe) { Fail '找不到 npx（node 装了但没有 npx？）。检查 PATH 里的 Node 安装是否完整。' }

# ---------------------------------------------------------------- 1 依赖

if ($env:SKIP_DEPS -eq '1') {
    Say '1/5 安装前端依赖（SKIP_DEPS=1，跳过）'
} else {
    Say '1/5 安装前端依赖'
    # 刻意用 install 而不是 ci：ci 会先清空 node_modules，
    # 那一步在本机容易被文件批量删除保护拦下而中断。
    & $npmExe install --no-audit --no-fund
    Assert-LastExit 'npm install'
}

# ---------------------------------------------------------------- 2 反代 exe

Say '2/5 打包反代服务（单文件 exe）'
if ($env:SKIP_REVERSE_PROXY -eq '1') {
    Warn 'SKIP_REVERSE_PROXY=1，跳过反代打包；下面 tauri build 会因为缺资源而失败'
} else {
    $null = Invoke-Capture $Py @('-c', 'import PyInstaller')
    if ($script:LastNativeCode -ne 0) {
        Info '安装 PyInstaller 与反代依赖…'
        & $Py -m pip install --disable-pip-version-check -q -r reverse-proxy/requirements.txt pyinstaller
        Assert-LastExit 'pip install'
    }
    Push-Location reverse-proxy
    try {
        & $Py build-exe.py
        Assert-LastExit 'build-exe.py'
    } finally {
        Pop-Location
    }
    $RpExe = 'reverse-proxy/dist/wb-reverse-proxy.exe'
    if (-not (Test-Path $RpExe)) { Fail "反代 exe 未生成：$RpExe" }
    Ok "$RpExe ($(Get-SizeMB $RpExe))"
    # 必须等它被独占结束（第 4 步的 cargo 构建脚本要读它算 rerun-if-changed 哈希）
    Wait-Released -Path (Resolve-Path $RpExe).Path
    Info '它会被 src-tauri/tauri.windows.conf.json 声明进安装目录根'
}

# ---------------------------------------------------------------- 3 让出 dist

Say '3/5 准备前端产物目录'
if (Test-Path dist) {
    $old = 'dist.old-' + (Get-Date -Format 'yyyyMMdd-HHmmss')
    Move-Item dist $old
    Info "旧 dist 已挪到 $old（不删除，避开批量删除保护；确认构建正常后可自行清理）"
} else {
    Info '没有旧 dist，跳过'
}

# ---------------------------------------------------------------- 4 tauri build

Say '4/5 构建桌面端（NSIS 安装包）'
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $Key -Raw).Trim()
if (-not $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) {
    # 与 CI 侧 GitHub Secret TAURI_SIGNING_PRIVATE_KEY_PASSWORD 保持一致；
    # 轮换密钥时两处要一起改（2026-10-31 轮换后为 wb-switch-ci）。
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = 'wb-switch-ci'
}
Info 'beforeBuildCommand 会自动跑 npm run build:desktop（前端 + Companion + agent-studio-runtime）'
# 反代 exe 的 mtime 要比源码新，否则装进包里的是旧版本——这里顺带提醒
if (Test-Path 'reverse-proxy/dist/wb-reverse-proxy.exe') {
    $exeTime = (Get-Item 'reverse-proxy/dist/wb-reverse-proxy.exe').LastWriteTime
    $newerPy = @(Get-ChildItem reverse-proxy -Recurse -Filter *.py -File -ErrorAction SilentlyContinue |
        Where-Object { $_.LastWriteTime -gt $exeTime } | Select-Object -First 3)
    if ($newerPy.Count -gt 0) {
        Warn "反代源码比 exe 新，建议重跑第 2 步：$($newerPy.FullName -join '；')"
    }
}
& $npxExe tauri build --bundles nsis
Assert-LastExit 'tauri build'

# ---------------------------------------------------------------- 5 汇总

Say '5/5 完成'

$BundleDir = $null
foreach ($d in @('src-tauri/target/release/bundle/nsis', 'target/release/bundle/nsis')) {
    if (Test-Path $d) { $BundleDir = (Resolve-Path $d).Path; break }
}
if (-not $BundleDir) { Fail '构建结束但没找到 NSIS 产物目录，请检查上面的日志' }

# 按「产品名_版本」精确找，别用「按名字排序取最后一个」：那是字典序，
# WB-Switch-Plus_0.1.0_ 会排在 WB-Switch-Plus_0.1.60_ 前面，
# 会把上一次构建的旧包报成新产物。
$installer = $null
$expected = Join-Path $BundleDir "${ProductName}_${AppVersion}_x64-setup.exe"
if ($ProductName -and $AppVersion -and (Test-Path $expected)) {
    $installer = $expected
} else {
    # 名字对不上（改了 productName、或平台后缀不是 x64）时退回「最新的那个」
    $latest = @(Get-ChildItem $BundleDir -Filter '*-setup.exe' -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1)
    if ($latest.Count -gt 0) {
        $installer = $latest[0].FullName
        if (-not (Test-Path $expected)) {
            Warn "没找到预期文件 ${ProductName}_${AppVersion}_x64-setup.exe，改用最新的 $($latest[0].Name)"
        }
    }
}
if (-not $installer) { Fail '构建结束但没找到 NSIS 安装包，请检查上面的日志' }

# 同一个目录里可能还躺着历史版本的安装包，明确提醒一下，免得拿错文件去发布
$stale = @(Get-ChildItem $BundleDir -Filter '*-setup.exe' -File -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -ne $installer })
if ($stale.Count -gt 0) {
    Warn "同目录还有历史安装包（发布时别拿错）：$($stale.Name -join ' ')"
}

Write-Host ('    安装包      : {0} ({1})' -f $installer, (Get-SizeMB $installer))
$sigFile = "$installer.sig"
if (Test-Path $sigFile) { Write-Host "    更新签名    : $sigFile" }

foreach ($d in @('target/release', 'src-tauri/target/release')) {
    $appExe = Join-Path $d 'wb-switch-rust.exe'
    if (Test-Path $appExe) { Write-Host "    裸可执行文件: $appExe"; break }
}

Write-Host ''
Write-Host '    发布前请跑一次产物自检：  npm run verify:release'
exit 0
