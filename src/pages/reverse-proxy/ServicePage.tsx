import { BuiltinServiceCard } from "@/components/reverse-proxy/builtin-service-card";

/**
 * 服务设置页 = 内置服务卡片（状态 / 启停 / 端口 / 监听范围 / 数据目录）。
 *
 * 旧的 `ServiceSettingsPanel`（site_name 表单）是 Python 网页版遗留——
 * `site_name` 是网页后台的 `<title>` 字段，桌面端没有消费方（附录 D），
 * 因此不再挂在内置页面；该组件本身保留给「反代服务old」。
 */
export default function ServicePage() {
  return (
    <div className="space-y-6">
      <BuiltinServiceCard />
    </div>
  );
}
