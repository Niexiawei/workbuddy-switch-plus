#!/usr/bin/env python3
"""把反代服务打成单文件 exe（Windows）。

    python build-exe.py                 # 单文件：dist/wb-reverse-proxy.exe
    python build-exe.py --onedir        # 目录形态：dist/wb-reverse-proxy/
    python build-exe.py --clean         # 先清 build/ 与 dist/

产物运行：
    双击 dist/wb-reverse-proxy.exe，控制台会打印管理地址与（首次的）管理员口令；
    运行数据落在 exe 同级的 data/ 目录，拷走整个文件夹即可换机使用。

依赖：pip install -r requirements.txt pyinstaller
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

# CI 的 Windows runner 上 stdout 默认是 cp1252，中文打印会直接
# UnicodeEncodeError（GitHub Actions 上实测过）。在第一个 print 之前
# 强制切到 UTF-8；Python 3.7+ 支持，失败（被重定向等场景）就随它去。
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[union-attr]
except (AttributeError, ValueError):
    pass

ROOT = Path(__file__).resolve().parent
SPEC = ROOT / "wb-reverse-proxy.spec"
NAME = "wb-reverse-proxy"

#: 版本号注入用的环境变量名。必须与 admin/version.py 的 VERSION_ENV 一致。
VERSION_ENV = "WB_REVERSE_PROXY_VERSION"

#: 随包携带的版本文件。spec 的 datas 里会把它打进冻结目录，
#: 因为**构建时的环境变量不会留在 exe 里**（见 admin/version.py 的说明）。
VERSION_FILE = "version.json"

#: 仓库根的 package.json —— 版本的唯一来源。
#: 为什么不在这里单独维护一个版本：反代和桌面端**发同一个版本号**，
#: 两处各写一份的必然结果是某次发版只改了一边，而症状是版本号静静地对不上。
PACKAGE_JSON = ROOT.parent / "package.json"


def read_version() -> str:
    """从 package.json 读版本。读不到就明确说明原因，不静默用占位值。

    打出来的 exe 会被单独部署到服务器上，事后要能回答「这是哪个版本」。
    如果这里静默降级成 "dev"，那个 exe 就永远说不清自己是什么时候的。
    """
    try:
        data = json.loads(PACKAGE_JSON.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"[build-exe] 找不到 {PACKAGE_JSON}，无法确定版本号")
    except ValueError as exc:
        raise SystemExit(f"[build-exe] {PACKAGE_JSON} 不是合法 JSON：{exc}")
    value = str(data.get("version") or "").strip()
    if not value:
        raise SystemExit(f"[build-exe] {PACKAGE_JSON} 里没有 version 字段")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description="打包反代服务为 exe")
    parser.add_argument("--onedir", action="store_true", help="产出目录形态（启动更快、体积更分散）")
    parser.add_argument("--clean", action="store_true", help="先删除 build/ 与 dist/")
    parser.add_argument("--version", default=None,
                        help=f"覆盖版本号（默认从 {PACKAGE_JSON.name} 读）")
    args = parser.parse_args()

    version = args.version or read_version()

    try:
        import PyInstaller  # noqa: F401
    except ImportError:
        print("缺少 PyInstaller。请先运行：", file=sys.stderr)
        print(f"  {sys.executable} -m pip install pyinstaller", file=sys.stderr)
        return 2

    if args.clean:
        for folder in ("build", "dist"):
            path = ROOT / folder
            if path.exists():
                shutil.rmtree(path)
                print(f"[build-exe] 已清理 {path}")

    env = dict(os.environ)
    if args.onedir:
        env["WB_EXE_ONEDIR"] = "1"
    # ⚠️ 环境变量**不足以**把版本带进 exe：os.environ 是运行期读的，
    # 构建时设的值只影响构建进程，不会写进产物（实测过，见 admin/version.py）。
    # 所以真正的手段是写一个 version.json 让 spec 打进包里；
    # 环境变量仍然设上，作为构建日志的可见证据与运行期的覆盖手段。
    env[VERSION_ENV] = version
    (ROOT / VERSION_FILE).write_text(
        json.dumps({"version": version}, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8")
    print(f"[build-exe] 版本号 {version}（来源 {PACKAGE_JSON.name}）")
    print(f"[build-exe] 已写入 {ROOT / VERSION_FILE}，将由 spec 打进包内")

    cmd = [sys.executable, "-m", "PyInstaller", "--noconfirm", str(SPEC)]
    print("[build-exe] " + " ".join(cmd))
    result = subprocess.run(cmd, cwd=ROOT, env=env)
    if result.returncode != 0:
        print("[build-exe] 打包失败", file=sys.stderr)
        return result.returncode

    target = ROOT / "dist" / (NAME + ("" if args.onedir else ".exe"))
    print()
    if target.exists():
        size = target.stat().st_size if target.is_file() else sum(
            f.stat().st_size for f in target.rglob("*") if f.is_file()
        )
        print(f"[build-exe] 完成：{target}（{size / 1024 / 1024:.1f} MB）")
        print(f"[build-exe] 版本：{version}")
        print(f"[build-exe] 直接运行：{target}")
    else:
        print(f"[build-exe] 打包结束，但没找到 {target}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
