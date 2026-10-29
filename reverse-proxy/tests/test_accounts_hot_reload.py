"""账号库热加载：workbuddy-switch 里增删账号后，服务无需重启即可生效。

要点：
  · 账号的**存在性**以账号库为权威（新增即出现、删除即消失）；
  · 「启用 / 暂停 / 改名 / 从本服务移除」是**本服务的视图**，存在覆盖层，
    不能被下一次账号库刷新抹掉；
  · 账号库里瞬时读不到（正在写 / 半截内容）时**必须保留现状**，不能当成全删。
"""
import json
import os
import tempfile
import time
import unittest
from pathlib import Path

import httpx

from fastapi import HTTPException

import admin.server as admin_server
from admin.pool import AccountPool
from admin.switch_store import SwitchStore, variant_of

ADMIN_KEY = "admin-test-credential-long-enough"


def record(uid, name, variant="cn"):
    return {"uid": uid, "nickname": name, "enterpriseId": "ent-" + uid,
            "access_token": "token-" + uid, "refresh_token": "refresh-" + uid,
            "expiresAt": int(time.time() * 1000) + 3600000, "variant": variant,
            "domain": "www.codebuddy.cn"}


class HotReloadSuite(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.accounts = self.root / "accounts.json"
        self._tick = time.time_ns()
        self.write_accounts([record("u1", "一号"), record("u2", "二号")])
        self.store = SwitchStore(
            root=self.root / "data",
            accounts_path=self.accounts,
            admin_key=ADMIN_KEY,
        )
        self.app = admin_server.create_app(store=self.store)
        self.client = httpx.AsyncClient(transport=httpx.ASGITransport(app=self.app),
                                        base_url="https://console.test")
        r = await self.client.post("/admin/api/login", json={"username": "admin", "password": ADMIN_KEY})
        self.assertEqual(r.status_code, 200, r.text)
        self.token = r.json()["token"]
        self.client.headers["X-Admin-Token"] = self.token

    async def asyncTearDown(self):
        await self.client.aclose()
        self.temp.cleanup()

    def write_accounts(self, records):
        self.accounts.write_text(json.dumps(records, ensure_ascii=False), encoding="utf-8")
        # Windows 文件时间戳粒度可能较粗，显式推进 mtime，保证指纹一定变化。
        self._tick += 1_000_000_000
        os.utime(self.accounts, ns=(self._tick, self._tick))

    async def rows(self):
        r = await self.client.get("/admin/api/pool/summary")
        self.assertEqual(r.status_code, 200, r.text)
        return {row["id"]: row for row in r.json()["accounts"]}

    # ── 新增 / 删除 ─────────────────────────────────────────────
    async def test_new_account_appears_without_restart(self):
        self.assertEqual(set(await self.rows()), {"u1", "u2"})
        self.write_accounts([record("u1", "一号"), record("u2", "二号"), record("u3", "三号", "ai")])
        rows = await self.rows()
        self.assertEqual(set(rows), {"u1", "u2", "u3"})
        self.assertEqual(rows["u3"]["pool_state"], "available")
        # 账号行里的 realm 现在统一走 realm.realm_of，返回 cn / global。
        # 这里原来是 "ai"（旧口径 variant_of 的返回值）——同一个账号在「账号行」
        # 叫 ai、在「签到与积分分派」叫 global，前端得同时认两套拼写，迟早漏一处。
        self.assertEqual(rows["u3"]["realm"], "global")  # variant=ai → realm=global
        # 国际版没有签到体系，界面靠这个字段把签到按钮标成「该版本无签到」。
        self.assertEqual(rows["u3"]["checkin_supported"], False)

    async def test_removed_account_and_its_state_are_dropped(self):
        # 给 u2 造一些历史状态与亲和绑定，删除后应当一并回收
        with self.store.lock:
            self.store.data["account_status"]["u2"] = {"request_count": 7, "cooldown_until": 0}
            self.store.data["session_bindings"]["abc"] = {"account_id": "u2", "expires": time.time() + 600}
            self.store.save()
        self.write_accounts([record("u1", "一号")])
        rows = await self.rows()
        self.assertEqual(set(rows), {"u1"})
        self.assertNotIn("u2", self.store.data["account_status"])
        self.assertEqual(self.store.data["session_bindings"], {})
        self.assertNotIn("u2", self.store.managers)

    async def test_active_reassigned_when_active_account_vanishes(self):
        with self.store.lock:
            self.store.data["active"] = "u2"
            self.store.save()
            self.store.sync_active()
        self.write_accounts([record("u1", "一号"), record("u3", "三号")])
        await self.rows()
        self.assertEqual(self.store.data["active"], "u1")
        # 数据面的凭据也要跟着换人
        from core import converter
        self.assertIs(converter.CONFIG["cred"], self.store.managers["u1"])

    # ── 本地覆盖不被刷新抹掉 ────────────────────────────────────
    async def test_pause_and_rename_survive_refresh(self):
        r = await self.client.request("PATCH", "/admin/api/accounts/u1",
                                     json={"enabled": False, "name": "我的主力号"})
        self.assertEqual(r.status_code, 200, r.text)
        rows = await self.rows()
        self.assertFalse(rows["u1"]["enabled"])
        self.assertEqual(rows["u1"]["name"], "我的主力号")

        # 账号库换了个新账号（内容变了 → 全量重载），覆盖仍然生效
        self.write_accounts([record("u1", "一号（账号库里改了昵称）"), record("u2", "二号"), record("u4", "四号")])
        self.store.refresh_accounts(force=True)
        rows = await self.rows()
        self.assertEqual(rows["u1"]["name"], "我的主力号")
        self.assertFalse(rows["u1"]["enabled"])
        self.assertEqual(rows["u2"]["name"], "二号")
        self.assertIn("u4", rows)

    async def test_activation_works_on_switch_library(self):
        """`active` 校验要认账号库，而不是去读「凭据文件」（旧 Store 的假设）。"""
        r = await self.client.request("PATCH", "/admin/api/accounts/u2", json={"active": True})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(self.store.data["active"], "u2")
        self.assertEqual((await self.rows())["u2"]["active"], True)

    async def test_service_side_removal_does_not_touch_library(self):
        r = await self.client.delete("/admin/api/accounts/u2")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json().get("scope"), "service")
        self.assertEqual(set(await self.rows()), {"u1"})
        # 账号库原封不动（服务只读账号库）
        on_disk = [rec["uid"] for rec in json.loads(self.accounts.read_text(encoding="utf-8"))]
        self.assertEqual(on_disk, ["u1", "u2"])
        # 再刷新也不会把它「复活」
        self.store.refresh_accounts(force=True)
        self.assertEqual(set(await self.rows()), {"u1"})
        # 但账号库里的凭据理论上仍可用：覆盖层只隐藏调度视图
        self.assertIn("u2", json.dumps(json.loads(self.accounts.read_text(encoding="utf-8"))))

    # ── 瞬时读不到时不能误删 ────────────────────────────────────
    async def test_transient_unreadable_keeps_existing_accounts(self):
        self.accounts.write_text("{ 半截内容", encoding="utf-8")
        result = self.store.refresh_accounts(force=True)
        self.assertTrue(result["stale"])
        self.assertFalse(result["changed"])
        self.assertEqual(set(await self.rows()), {"u1", "u2"})

        # 恢复后重新加载
        self.write_accounts([record("u1", "一号"), record("u2", "二号"), record("u5", "五号")])
        self.assertEqual(set(await self.rows()), {"u1", "u2", "u5"})

    async def test_missing_library_is_empty_not_stale(self):
        self.accounts.unlink()
        rows = await self.rows()
        self.assertEqual(rows, {})
        self.assertEqual(self.store.data["active"], None)

    async def test_removal_applies_even_when_library_unreadable(self):
        """移除只针对已知 id，不该被「账号库此刻读不到」拖住。"""
        self.accounts.write_text("{ 半截", encoding="utf-8")
        r = await self.client.delete("/admin/api/accounts/u2")
        self.assertEqual(r.status_code, 200, r.text)
        self.assertNotIn("u2", await self.rows())
        # 账号库恢复后也不复活（覆盖层记着）
        self.write_accounts([record("u1", "一号"), record("u2", "二号")])
        self.assertEqual(set(await self.rows()), {"u1"})

    # ── 指纹短路 ────────────────────────────────────────────────
    async def test_unchanged_library_is_not_reloaded(self):
        self.store.refresh_accounts()
        result = self.store.refresh_accounts()
        self.assertFalse(result["changed"])
        self.assertFalse(result["stale"])
        self.assertEqual(result["total"], 2)


    # ── 需重新登录 ──────────────────────────────────────────────
    async def test_needs_relogin_account_is_invalid(self):
        """桌面端 refresh 失败后写的 needs_relogin 必须被反代认成「不可用」。

        否则同一个账号在账户管理页显示「需重新登录」、在反代页显示「可用」，
        请求继续打到已经失效的凭据上。
        """
        self.write_accounts([
            record("u1", "一号"),
            dict(record("u2", "二号"), needs_relogin=True, needs_relogin_reason="缺少 refresh token"),
        ])
        rows = await self.rows()
        self.assertEqual(rows["u2"]["pool_state"], "invalid")
        self.assertTrue(rows["u2"]["needs_relogin"])
        self.assertEqual(rows["u1"]["pool_state"], "available")

        # 选号同样跳过它：把 u1 暂停后池里就没有可用账号了。
        self.store.data["accounts"]["u1"]["enabled"] = False
        pool = AccountPool(self.store, clock=lambda: time.time())
        with self.assertRaises(HTTPException):
            pool.select()

class VariantSuite(unittest.TestCase):
    def test_variant_to_realm(self):
        self.assertEqual(variant_of({"variant": "cn"}), "cn")
        self.assertEqual(variant_of({"variant": "ai"}), "ai")
        self.assertEqual(variant_of({"realm": "global"}), "ai")
        self.assertEqual(variant_of({}), "cn")
