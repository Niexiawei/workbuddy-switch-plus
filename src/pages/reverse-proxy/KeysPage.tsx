import { KeysPanel } from "@/components/reverse-proxy/keys-panel";
import { useReverseProxy } from "./ReverseProxyLayout";
import { PanelSkeleton } from "./panel-gate";

export default function KeysPage() {
  const { overview, reloadOverview } = useReverseProxy();
  if (!overview) return <PanelSkeleton />;
  return <KeysPanel overview={overview} reload={reloadOverview} />;
}