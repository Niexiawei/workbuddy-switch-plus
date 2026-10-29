/**
 * 侧边栏导航项。
 *
 * 抽出来的原因很实际：原来每个 `NavLink` 都把同一串 className 复制一遍，
 * 已有五份；再加「反代服务」的二级菜单会变成十几份。这里把「激活 / 未激活」
 * 的样式收成一处，父项、子项、分组标题各一个变体。
 */

import { NavLink } from "react-router-dom";
import type { LucideIcon } from "lucide-react";

import { cn } from "@/lib/utils";

/** 顶层的主题径项。 */
export function SidebarNavItem({
  to,
  icon: Icon,
  label,
  end,
}: {
  to: string;
  icon: LucideIcon;
  label: string;
  end?: boolean;
}) {
  return (
    <NavLink
      to={to}
      end={end}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-2.5 rounded-lg px-3 py-2.5 text-sm outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
          isActive
            ? "bg-foreground/[0.06] font-medium text-foreground"
            : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
        )
      }
    >
      <Icon className="size-4 shrink-0" />
      <span className="min-w-0 truncate">{label}</span>
    </NavLink>
  );
}

/**
 * 二级菜单的子项。
 *
 * 比顶层项小一号、缩进更多：两级之间靠字号 + 缩进区分，而不是靠再加一层边框
 * （侧边栏已经有右边框，再套框会显得脏）。计数徽标用 `tabular-nums`，
 * 数字跳动时不会左右抖。
 */
export function SidebarSubItem({
  to,
  label,
  count,
}: {
  to: string;
  label: string;
  count?: number;
}) {
  return (
    <NavLink
      to={to}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-2 rounded-md py-2 pr-2 pl-9 text-[13px] outline-none transition-colors focus-visible:ring-2 focus-visible:ring-sidebar-ring/50",
          isActive
            ? "bg-foreground/[0.06] font-medium text-foreground"
            : "text-muted-foreground hover:bg-foreground/[0.04] hover:text-foreground",
        )
      }
    >
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {count !== undefined ? (
        <span className="text-[10px] tabular-nums text-muted-foreground/80">{count}</span>
      ) : null}
    </NavLink>
  );
}

/** 子菜单里的小标题（「高级」）。 */
export function SidebarGroupLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-2 pt-2 pb-1 pl-9 text-[10px] font-medium tracking-wider text-muted-foreground/70 uppercase">
      {children}
    </div>
  );
}
