import { UsersPanel } from "@/components/reverse-proxy/users-panel";
import { useReverseProxy } from "./ReverseProxyLayout";

/**
 * 后台账号。
 *
 * 内置服务用宿主令牌鉴权，没有「第二个后台账号体系」；这个页面在桌面端
 * 实际只能看到默认的 admin。可见性由 I4 的取舍决定，这里保留承载。
 */
export default function UsersPage() {
  const { users, reloadAccess } = useReverseProxy();
  return (
    <UsersPanel
      users={users}
      session={{ username: "admin", role: "admin" }}
      reload={reloadAccess}
      onRelogin={() => undefined}
    />
  );
}