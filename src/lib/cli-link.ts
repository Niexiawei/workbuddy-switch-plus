/**
 * codeBuddy CLI 接入卡片里能脱离 DOM 测试的那部分。
 *
 * 卡片本身是纯展示 + 调接口，把「地址怎么归一化」单独拿出来，是为了让
 * 这类容易出错的字符串处理能被单测钉住（vitest 当前是 node 环境，不渲染组件）。
 */

/**
 * 归一化用户填写的反代地址。
 *
 * CLI 会把 `CODEBUDDY_BASE_URL` 原样当 OpenAI baseURL，再在末尾拼
 * `/chat/completions`——少了版本段就会打到门户根路径（实测返回 405）。
 * 所以：
 *   · 去掉首尾空白与结尾斜杠；
 *   · 已经带 `/v1`、`/v2` 这类版本段的原样保留；
 *   · 其余统一补 `/v1`；
 *   · 空串返回空串，交给调用方禁用按钮，不要在这里编一个默认地址。
 */
export function normalizeProxyBase(value: string): string {
  const trimmed = value.trim().replace(/\/+$/, "");
  if (!trimmed) return "";
  return /\/v\d+$/.test(trimmed) ? trimmed : `${trimmed}/v1`;
}
