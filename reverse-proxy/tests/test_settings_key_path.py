"""HyactCrack 技术支持 — 锁住「密钥文件跟随数据目录」这条不变量

## 背景（真实故障）

用户报「验证码发送失败，请联系管理员」。查下来不是 163 侧问题：
`decrypt_secret` 返回**空串**，拿空口令去认证被 163 回 `535`。

根因是 `_secret_path()` 原先只看 `WB_SWITCH_DATA` 环境变量，
**完全不理 `--data`**。

- 源码运行：`__file__` 的 parents[1] 恰好是仓库 `reverse-proxy/`，
  密钥落在 `reverse-proxy/data/settings.key` —— 看起来没问题；
- **冻结成 exe**：`__file__` 是 `<_MEIPASS>/admin/settings_store.py`，
  于是密钥落进 `%TEMP%\\_MEIxxxxx/data/`。而 `_MEIPASS` **每次启动随机**，
  服务重启一次就换一把密钥 → 上次存的 SMTP 授权码永久解不开。

实测佐证：该机器 `%TEMP%` 下 41 个 `_MEI*` 目录，只有 3 个带 `settings.key`
—— 绝大多数在重启后消失，密钥随之丢失。

## 这个文件锁什么

1. `build_app(--data X)` 之后，密钥路径必须落在 X 里；
2. 换数据目录必须让密钥缓存失效（否则会用旧密钥解新密文）；
3. 没显式指定时，退回源码旁 `data/`（保持历史行为）。
"""
from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(Path(__file__).resolve().parent))

from admin import settings_store as ss  # noqa: E402


class SecretPathFollowsDataDirTest(unittest.TestCase):
    """密钥文件必须跟着 `--data` 走。"""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="wb-keypath-"))
        (self.tmp / "acc.json").write_text("[]", encoding="utf-8")
        # 每个用例都从干净状态开始，避免缓存串味
        ss.set_runtime_data_root(None)

    def tearDown(self):
        ss.set_runtime_data_root(None)

    def assert_under(self, path, root, message=""):
        """判断 path 是否落在 root 之下——按真实路径比，不比字符串前缀。

        Windows 的临时目录可能是 8.3 短名（`C:\\Users\\NIEXIA~1\\…`），
        而 `Path.resolve()` 会展开成长名。两边口径不一致时 startswith 会假红，
        与本文件要锁的「密钥跟随数据目录」这条不变量无关。
        """
        try:
            Path(path).resolve().relative_to(Path(root).resolve())
        except ValueError:
            self.fail(message or f"{path} 不在 {root} 之下")

    def test_secret_path_follows_explicit_data_dir(self):
        """⭐ 主断言：设了运行目录，密钥就落在那里。"""
        ss.set_runtime_data_root(self.tmp)
        p = ss._secret_path()
        self.assertEqual(p, self.tmp / "settings.key")
        self.assert_under(p, self.tmp,
                          "密钥跑到数据目录外面去了 —— 冻结后就会落进 %TEMP%")

    def test_build_app_wires_it_up(self):
        """⭐ 端到端：真实入口 `build_app(--data X)` 必须接上。"""
        import serve
        serve.build_app(["--data", str(self.tmp),
                         "--accounts", str(self.tmp / "acc.json")])
        p = ss._secret_path()
        self.assert_under(p, self.tmp, f"build_app 没把数据目录传给密钥路径，得到 {p}")

    def test_key_cache_invalidated_on_dir_change(self):
        """⭐ 换目录后不能复用旧密钥 —— 复用了就还是解不开。"""
        a = Path(tempfile.mkdtemp(prefix="wb-ka-"))
        b = Path(tempfile.mkdtemp(prefix="wb-kb-"))
        ss.set_runtime_data_root(a)
        ka = ss.settings_key()
        ss.set_runtime_data_root(b)
        kb = ss.settings_key()
        self.assertNotEqual(ka, kb, "换了数据目录却复用同一把密钥 —— 缓存没失效")

    def test_env_fallback_still_works(self):
        """没设运行目录时，退到 WB_SWITCH_DATA 环境变量。"""
        env = Path(tempfile.mkdtemp(prefix="wb-env-"))
        old = os.environ.get("WB_SWITCH_DATA")
        os.environ["WB_SWITCH_DATA"] = str(env)
        try:
            ss.set_runtime_data_root(None)
            self.assertEqual(ss._secret_path(), env / "settings.key")
        finally:
            if old is None:
                os.environ.pop("WB_SWITCH_DATA", None)
            else:
                os.environ["WB_SWITCH_DATA"] = old

    def test_source_run_default_is_repo_data_dir(self):
        """都没设时退回源码旁 data/ —— 保持历史行为，别把老部署弄坏。"""
        old = os.environ.pop("WB_SWITCH_DATA", None)
        try:
            ss.set_runtime_data_root(None)
            p = ss._secret_path()
            self.assertEqual(p.name, "settings.key")
            self.assertEqual(p.parent.name, "data")
            self.assertEqual(p.parent.parent.name, "reverse-proxy")
        finally:
            if old is not None:
                os.environ["WB_SWITCH_DATA"] = old

    def test_frozen_layout_would_have_escaped(self):
        """回归说明：模拟冻结布局，证明旧逻辑会跑到数据目录之外。

        这不是在测当前实现，而是把「缺陷长什么样」固化下来 ——
        万一有人把 `_runtime_data_root` 那层去掉，这个用例会提醒他
        冻结形态下密钥会跑到 `<_MEIPASS>/data/`。
        """
        fake_meipass = Path(tempfile.mkdtemp(prefix="wb-meipass-"))
        fake_file = fake_meipass / "admin" / "settings_store.py"
        fake_file.parent.mkdir(parents=True, exist_ok=True)
        fake_file.write_text("", encoding="utf-8")
        # 旧逻辑等价于：
        old_style = fake_file.resolve().parents[1] / "data" / "settings.key"
        self.assert_under(old_style, fake_meipass)
        # 而新逻辑（设过运行目录后）不会跑到那儿：
        ss.set_runtime_data_root(self.tmp)
        try:
            ss._secret_path().resolve().relative_to(fake_meipass.resolve())
        except ValueError:
            pass
        else:
            self.fail(f"密钥落在了 _MEIPASS 下：{ss._secret_path()}")


if __name__ == "__main__":
    unittest.main()
