import { useNavigate } from "react-router-dom";

import { OverviewPanel } from "@/components/reverse-proxy/overview-panel";
import { useReverseProxy } from "./ReverseProxyLayout";
import { PanelSkeleton } from "./panel-gate";

export default function OverviewPage() {
  const { overview, reloadOverview } = useReverseProxy();
  const navigate = useNavigate();
  if (!overview) return <PanelSkeleton />;
  return (
    <OverviewPanel
      overview={overview}
      onReload={reloadOverview}
      // onGoto 原来切的是页内 Tab，现在改成真的换路由：可直达、可刷新。
      onGoto={(tab) => navigate(`/reverse-proxy/${tab}`)}
    />
  );
}