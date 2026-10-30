#!/usr/bin/env node
/**
 * release 产物自检：确保二进制里没有残留「构建机的绝对路径」。
 *
 * 为什么需要这个检查
 * ------------------
 * `env!("CARGO_MANIFEST_DIR")` 是**编译期**常量，值是打包这台机器上的绝对路径。
 * 一旦它被用于运行时的 `PathBuf::from(...)`（而不是 `include_bytes!` 那种纯编译期
 * 读取），整条路径就会原样写进可执行文件。后果是：在开发机上一切正常（路径正好存在），
 * 到了用户机器上，「找不到资源」的兜底会静默指向某个早已不存在、或者更糟——
 * 恰好还存在但不该被使用的开发目录。
 *
 * 这类问题靠肉眼看代码很容易漏（`include_bytes!` 的同名用法是安全的），
 * 所以打包完必须回头扫一遍产物。
 *
 * 用法
 * ----
 *   node scripts/verify-release-binary.mjs [可选：产物路径...]
 * 默认检查桌面端 exe 与反代服务 exe；有命中即以退出码 1 失败。
 */

import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const workspace = dirname(root); // 仓库根，以及它所在的目录，两者都不该出现在产物里

const defaults = [
  "target/release/wb-switch-rust.exe",
  "target/release/wb-switch-rust",
  "reverse-proxy/dist/wb-reverse-proxy.exe",
];
const targets = (process.argv.length > 2 ? process.argv.slice(2) : defaults).map((p) =>
  resolve(root, p),
);

/** 二进制里路径以 `\` 分隔（Windows）或 `/`（其它平台），两种形态都查。 */
function needles() {
  const out = new Set();
  for (const dir of [root, workspace]) {
    out.add(dir);
    out.add(dir.replaceAll("\\", "/"));
    out.add(dir.replaceAll("/", "\\"));
  }
  return [...out].filter((text) => text.length > 3);
}

const list = needles();
let failed = false;
let checked = 0;

for (const target of targets) {
  if (!existsSync(target)) {
    console.log(`跳过（不存在）：${target}`);
    continue;
  }
  checked += 1;
  const size = statSync(target).size;
  const bytes = readFileSync(target);
  const hits = [];
  for (const needle of list) {
    const raw = Buffer.from(needle, "utf8");
    let at = bytes.indexOf(raw);
    // 同一个 needle 可能命中多处（例如若干条兜底路径共享同一个前缀），全部列出。
    while (at >= 0) {
      // 截一段上下文，方便判断是哪种用法漏出来的。
      const from = Math.max(0, at - 60);
      const to = Math.min(bytes.length, at + raw.length + 80);
      const context = bytes
        .subarray(from, to)
        .toString("latin1")
        .replaceAll(/[^\x20-\x7e]+/g, " ");
      // panic 定位串豁免：rustc 会把 `file!()`（panic 的 Location）编译成
      // 绝对路径，Linux/Mac 的 CI 构建里所有 panic 消息都带
      // `/home/runner/work/<repo>/<repo>/...`。它只用于错误报告显示，
      // 不参与运行时的资源寻址，不属于本脚本要防的
      // 「env!("CARGO_MANIFEST_DIR") 被拿去 PathBuf::from」威胁模型。
      // 判据：panic Location 在二进制里的排布是 `…/file.rs` 后紧跟消息文本，
      // 命中的 needle 若以 .rs 结尾、或其前文紧邻 .rs 文件名，即视为定位串。
      const before = bytes.subarray(Math.max(0, at - 200), at).toString("latin1");
      const isPanicLocation =
        needle.endsWith(".rs") || /\.rs[^\x20-\x7e]/.test(before.slice(-80));
      if (!isPanicLocation) {
        hits.push({ needle, at, context });
      }
      at = bytes.indexOf(raw, at + raw.length);
    }
  }
  if (hits.length === 0) {
    console.log(`通过：${target}（${(size / 1048576).toFixed(1)} MB，未发现构建机路径）`);
    continue;
  }
  failed = true;
  console.error(`失败：${target} 里出现了构建机的绝对路径，共 ${hits.length} 处`);
  for (const hit of hits) {
    console.error(`  · 命中 ${hit.needle}`);
    console.error(`    上下文 …${hit.context}…`);
  }
}

if (failed) {
  console.error(
    "\n处理办法：把对应的 `env!(\"CARGO_MANIFEST_DIR\")` 兜底用 `#[cfg(debug_assertions)]`" +
      " 包起来（只让开发态生效），然后重新打包。",
  );
  process.exit(1);
}
console.log(`\n产物自检通过（检查了 ${checked} 个文件）。`);
