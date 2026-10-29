import { GuidePanel, TestPanel } from "@/components/reverse-proxy/test-guide-panel";
import { useReverseProxy } from "./ReverseProxyLayout";
import { PanelSkeleton } from "./panel-gate";

/**
 * 「接入指南」与「连接测试」合成一页。
 *
 * 两者讲的是同一件事的两半：先照着接，接不通就在这里测一次。
 * 连接测试要读概览里的模型清单，所以没有概览时只能先给骨架。
 */
export default function GuidePage() {
  const { overview } = useReverseProxy();
  if (!overview) return <PanelSkeleton />;
  const active = overview.accounts.find((account) => account.active && account.enabled);
  return (
    <div className="space-y-6">
      <GuidePanel />
      <TestPanel overview={overview} active={active} />
    </div>
  );
}
