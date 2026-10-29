import { RequestLogPanel } from "@/components/reverse-proxy/request-log-panel";
import { useReverseProxy } from "./ReverseProxyLayout";
import { PanelSkeleton } from "./panel-gate";

export default function LogsPage() {
  const { overview } = useReverseProxy();
  if (!overview) return <PanelSkeleton />;
  // 分组归属要靠 overview 里的 keys[].groups 与 groups[]（id→name），
  // 日志行本身只有 key_id。只传这两项，不把整个 overview 递进去。
  return <RequestLogPanel keys={overview.keys} groups={overview.groups} />;
}