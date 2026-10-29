import { TokensPanel } from "@/components/reverse-proxy/tokens-panel";
import { useReverseProxy } from "./ReverseProxyLayout";

export default function TokensPage() {
  const { tokens, reloadAccess } = useReverseProxy();
  return <TokensPanel tokens={tokens} reload={reloadAccess} />;
}