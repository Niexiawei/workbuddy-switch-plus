"""生成反代服务的「契约清单」：路由表 + 状态键表 + 契约文档。

为什么需要：Rust 重写（docs/reverse-proxy-rust-migration-plan.md）不能靠读代码猜接口，
先把「有哪些端点、谁鉴权、有没有副作用」变成机器可读的清单，后续 golden / 路由对齐
都对着它做。

用法（在 reverse-proxy/ 下）：

    ./.venv/Scripts/python.exe tools/record_contract.py

产物：
  tests/fixtures/proxy/manifest.json   路由清单（method/path/group/auth/risk）
  docs/reverse-proxy-contract.md       人读版契约（自动生成，勿手改表）

注意：本脚本只做**离线内省**（用临时目录建一个空 store），不连运行中的服务、
不写用户数据。
"""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]          # reverse-proxy/
ROOT = REPO.parent                                   # 仓库根
sys.path.insert(0, str(REPO))

from admin.server import create_app                   # noqa: E402
from admin.switch_store import SwitchStore            # noqa: E402


#: 只读但用 POST 表达的端点（轮询 / 查询类），不能按方法一律当写操作。
READ_LIKE_POST = {
    "/admin/api/oauth/{fid}/poll",
    "/admin/api/keys/check-models",
    "/admin/api/test",
}

#: 会打到上游、产生真实副作用（消耗积分 / 发邮件 / 改远端状态）的端点。
UPSTREAM_WRITE = {
    "/admin/api/oauth/start",
    "/admin/api/oauth/batch/start",
    "/admin/api/oauth/{fid}/poll",
    "/admin/api/accounts/{aid}/actions/{action}",
    "/admin/api/pool/actions/{action}",
    "/admin/api/alerts/test",
    "/admin/api/test",
}

#: 会删除/覆盖本地数据的端点；排障时最不该在「录制」阶段被误调。
DESTRUCTIVE = {
    "/admin/api/security/logs/clear",
    "/admin/api/audit-logs/clear",
    "/admin/api/pool/checkin-logs/clear",
    "/admin/api/users/{username}",
    "/admin/api/tokens/{tid}",
    "/admin/api/keys/{kid}",
    "/admin/api/security/rules/{rid}",
    "/admin/api/groups/{gid}",
    "/admin/api/egress/lines/{lid}",
    "/admin/api/accounts/{aid}",
    "/admin/api/cli/key",
    "/admin/api/oauth/{fid}",
}

#: 公开端点（不需要管理会话）。
PUBLIC = {
    "/", "/health", "/favicon.ico", "/admin", "/admin/", "/admin/assets/{filename}",
    "/admin/api/login", "/admin/api/login-options",
}


def group_of(path: str) -> str:
    if path.startswith("/v1/"):
        return "data-plane"
    if path.startswith("/admin/assets") or path in ("/", "/admin", "/admin/", "/favicon.ico"):
        return "static"
    parts = [part for part in path.split("/") if part]
    if len(parts) >= 3 and parts[0] == "admin" and parts[1] == "api":
        return parts[2]
    return parts[-1] if parts else "root"


def auth_of(path: str) -> str:
    if path in PUBLIC or path.startswith("/v1/") or path == "/health":
        return "none"
    return "admin"


def has_body(method: str, path: str) -> bool:
    if method in ("GET", "DELETE", "HEAD"):
        return path == "/admin/api/login"   # 登录用 body 传口令/key
    return True


def risk_of(method: str, path: str) -> str:
    if path in PUBLIC or method == "GET":
        return "read"
    if path in DESTRUCTIVE:
        return "destructive"
    if path in UPSTREAM_WRITE:
        return "upstream-write"
    if path in READ_LIKE_POST:
        return "read"
    return "local-write"


#: state.json 的键 → 含义与写入方。人工维护：跨版本必须保持兼容。
STATE_KEYS = [
    ("keys", "API 密钥（策略与用量）", "keysvc"),
    ("users", "后台账号（PBKDF2 口令）", "security"),
    ("api_tokens", "管理令牌（只存哈希）", "security"),
    ("audit_logs", "审计日志", "security"),
    ("security", "防爆破计数与会话配置", "security"),
    ("ip_rules", "IP 黑白名单", "security/server"),
    ("ip_access_logs", "IP 拦截日志", "server"),
    ("ip_last_allowed", "最近放行的 IP", "server"),
    ("api_key_ips", "密钥来源 IP 统计", "keysvc"),
    ("account_status", "账号运行态（冷却/积分/模型冷却）", "pool"),
    ("account_overrides", "账号本地覆盖", "switch_store"),
    ("session_bindings", "会话亲和表", "pool"),
    ("active", "当前账号（本服务视图）", "switch_store"),
    ("accounts", "账号运行时视图（不落盘）", "switch_store"),
    ("pool", "池配置", "pool"),
    ("groups", "分组", "groups"),
    ("egress", "出口线路与绑定", "egress"),
    ("alerts", "告警配置", "alerts"),
    ("usage_stats", "用量统计桶", "usage"),
    ("checkin_logs", "任务流水", "pool"),
    ("travel_states", "旅行状态", "travel"),
    ("runtime_since", "本次会话起点", "switch_store"),
    ("runtime_total_sec", "累计运行秒数", "switch_store"),
    ("runtime_sessions", "启动次数", "switch_store"),
    ("model_aliases", "模型别名", "server"),
    ("secret", "站点密钥", "settings_store"),
    ("settings", "站点设置", "settings_store"),
]


def build_manifest() -> dict:
    root = Path(tempfile.mkdtemp(prefix="p0-routes-"))
    accounts = root / "accounts.json"
    accounts.write_text("[]", encoding="utf-8")
    store = SwitchStore(root=str(root / "state"), accounts_path=str(accounts),
                        admin_key="a" * 40)
    app = create_app(store=store, admin_key="a" * 40)

    routes = []
    for route in app.routes:
        path = getattr(route, "path", "")
        methods = sorted(getattr(route, "methods", []) or [])
        if not methods:
            continue
        if not (path.startswith(("/admin", "/v1", "/health")) or path in ("/", "/favicon.ico")):
            continue
        for method in methods:
            routes.append({
                "method": method,
                "path": path,
                "group": group_of(path),
                "auth": auth_of(path),
                "body": has_body(method, path),
                "risk": risk_of(method, path),
            })
    store.stop_runtime_writer()
    routes.sort(key=lambda row: (row["group"], row["path"], row["method"]))
    return {
        "generated_by": "reverse-proxy/tools/record_contract.py",
        "source": "python reverse-proxy (pre-Rust)",
        "route_count": len(routes),
        "routes": routes,
        "state_keys": [{"key": k, "desc": d, "owner": o} for k, d, o in STATE_KEYS],
    }


def render_markdown(manifest: dict) -> str:
    rows = manifest["routes"]
    by_group: dict[str, list[dict]] = {}
    for row in rows:
        by_group.setdefault(row["group"], []).append(row)

    lines = [
        "# 反代服务契约（自动生成）",
        "",
        "> 由 `reverse-proxy/tools/record_contract.py` 生成，**不要手改表格**。",
        "> Rust 重写验收时，路由、鉴权、副作用等级必须与本文一致。",
        "",
        f"- 路由总数：**{len(rows)}**",
        f"- 分组：{', '.join(sorted(by_group))}",
        "- 风险：`read`（只读）/ `local-write`（只改本地状态）/ "
        "`upstream-write`（会打上游、可能扣积分或发邮件）/ `destructive`（删除或覆盖）",
        "",
        "## 1. 路由清单",
        "",
    ]
    for group in sorted(by_group):
        lines += [f"### {group}", "", "| 方法 | 路径 | 鉴权 | 有 body | 风险 |",
                  "| --- | --- | --- | --- | --- |"]
        for row in by_group[group]:
            lines.append(f"| {row['method']} | `{row['path']}` | {row['auth']} | "
                         f"{'是' if row['body'] else '否'} | {row['risk']} |")
        lines.append("")

    lines += ["## 2. state.json 键", "", "| 键 | 含义 | 写入方 |", "| --- | --- | --- |"]
    for item in manifest["state_keys"]:
        lines.append(f"| `{item['key']}` | {item['desc']} | {item['owner']} |")
    lines += [
        "",
        "## 3. 录制规则",
        "",
        "- **只读端点**（`read`）可直接采样响应，用于 golden 对比。",
        "- **local-write / destructive** 只记录请求形状，不在真实数据上执行。",
        "- **upstream-write** 不录制真实调用（会扣积分/发邮件）：用本地 mock 上游录制协议层。",
        "- 数据面样例见 `tests/fixtures/proxy/dataplane/`（本地 mock 生成，不消耗额度）。",
        "",
        "## 4. 已知缺口",
        "",
        "- 管理端点的响应 JSON Schema 未冻结：FastAPI 的 `/openapi.json` 只暴露 6 条数据面路由，"
        "管理路由是运行时注册的；只能靠后续 fixture 采样补齐。",
        "- 管理面鉴权失败路径（401/403/429）未覆盖，P5 落地时按安全不变量补。",
        "- 扫码登录只冻结状态机（state → poll → token），不录浏览器交互。",
        "",
        "## 5. 数据面 golden 样例",
        "",
    ]
    index_path = ROOT / "tests" / "fixtures" / "proxy" / "dataplane" / "index.json"
    if index_path.is_file():
        cases = json.loads(index_path.read_text(encoding="utf-8")).get("cases", [])
        lines += ["| 样例 | 覆盖点 |", "| --- | --- |"]
        for case in cases:
            if isinstance(case, dict):
                lines.append(f"| `{case.get('name')}` | {case.get('note', '')} |")
        lines.append("")
    lines += [
        "## 6. 如何重新生成",
        "",
        "在 `reverse-proxy/` 下（venv 里要有 fastapi / httpx / pytest）：",
        "",
        "```bash",
        "./.venv/Scripts/python.exe tools/record_contract.py    # 路由表 + 本文件",
        "./.venv/Scripts/python.exe tools/record_dataplane.py   # 数据面 golden",
        "```",
        "",
        "两个脚本都离线运行：前者用临时目录建空 store，后者用 MockTransport 拦上游，",
        "**不会**消耗账号积分、不会写真实 state.json。",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    manifest = build_manifest()
    out = ROOT / "tests" / "fixtures" / "proxy" / "manifest.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    doc = ROOT / "docs" / "reverse-proxy-contract.md"
    doc.write_text(render_markdown(manifest), encoding="utf-8")
    print(f"routes={manifest['route_count']} -> {out}")
    print(f"doc -> {doc}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
