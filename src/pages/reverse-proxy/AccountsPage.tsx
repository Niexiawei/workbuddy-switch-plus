import { AccountsPanel } from "@/components/reverse-proxy/accounts-panel";
import { useReverseProxy } from "./ReverseProxyLayout";
import { PanelSkeleton } from "./panel-gate";

export default function AccountsPage() {
  const { overview, reloadOverview, openAddAccount, openBatchScan } = useReverseProxy();
  if (!overview) return <PanelSkeleton />;
  return (
    <AccountsPanel
      overview={overview}
      reload={reloadOverview}
      onAddAccount={openAddAccount}
      onBatchScan={openBatchScan}
    />
  );
}