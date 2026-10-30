/**
 * 反代服务的子菜单定义。
 *
 * 与路由表分开是因为两者用途不同：路由要能用 `<Route>` 表达（含「高级」嵌套），
 * 菜单要知道**显示顺序与分组**。放在一处会变成「为了菜单好看去改路由嵌套」。
 *
 * 分组沿用旧页面的语义（运行 / 账号 / 接入 / 模型 / 服务 / 高级），
 * 但把原来的「运维」拆成「高级」——那里放的是低频、且桌面端多半用不上的入口。
 */

export type ReverseProxyNavItem = {
  /** 相对 `/reverse-proxy/` 的路径段。 */
  segment: string;
  label: string;
  /** 有计数徽标的项，从概览里取哪个数字。 */
  count?: "accounts" | "keys";
};

export type ReverseProxyNavGroup = {
  /** 分组标题；为空表示不显示标题（常用项）。 */
  title?: string;
  items: ReverseProxyNavItem[];
};

export const REVERSE_PROXY_NAV: ReverseProxyNavGroup[] = [
  {
    items: [
      { segment: "overview", label: "概览" },
      { segment: "logs", label: "请求日志" },
    ],
  },
  {
    items: [
      { segment: "accounts", label: "账号池", count: "accounts" },
      { segment: "groups", label: "分组" },
      { segment: "egress", label: "出口线路" },
    ],
  },
  {
    items: [
      { segment: "keys", label: "API 密钥", count: "keys" },
      { segment: "cli", label: "CodeBuddy CLI" },
      { segment: "guide", label: "接入指南" },
    ],
  },
  {
    items: [
      { segment: "models", label: "模型与用量" },
    ],
  },
  {
    items: [
      { segment: "service", label: "服务设置" },
      { segment: "alerts", label: "告警通知" },
    ],
  },
  {
    title: "高级",
    items: [
      { segment: "security", label: "IP 安全" },
    ],
  },
];

/** 默认落地页：侧边栏点「反代服务」时进哪一页。 */
export const REVERSE_PROXY_DEFAULT_SEGMENT = "overview";
