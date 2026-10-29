import { ServiceSettingsPanel } from "@/components/reverse-proxy/service-settings-panel";
import { BuiltinServiceCard } from "@/components/reverse-proxy/builtin-service-card";

export default function ServicePage() {
  return (
    <div className="space-y-6">
      <BuiltinServiceCard />
      <ServiceSettingsPanel />
    </div>
  );
}