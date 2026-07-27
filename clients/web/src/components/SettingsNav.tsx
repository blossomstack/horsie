import type { LucideIcon } from "lucide-react";
import { NavLink } from "react-router-dom";
import { cn } from "../lib/cn";
import { useSettingsDirty } from "../pages/settings/dirty";

export type NavItem = { to: string; label: string; icon: LucideIcon };

/** The second column of the settings/admin areas: a vertical page switcher. */
export function SettingsNav({
  title,
  items,
}: {
  title: string;
  items: NavItem[];
}) {
  const { confirmLeave } = useSettingsDirty();
  return (
    <nav
      className="flex h-full w-52 shrink-0 flex-col border-r"
      style={{ background: "var(--surface)" }}
      data-testid="settings-nav"
    >
      <div className="px-4 py-3.5 text-[15px] font-semibold tracking-tight text-text">
        {title}
      </div>
      <div className="space-y-0.5 px-2">
        {items.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
            end
            data-testid={`settings-nav-${to}`}
            onClick={(e) => {
              if (!confirmLeave()) e.preventDefault();
            }}
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2 rounded-[var(--radius)] px-2.5 py-2 text-sm transition-colors",
                isActive
                  ? "bg-surface-3 text-text"
                  : "text-muted hover:bg-surface-2 hover:text-text",
              )
            }
          >
            <Icon size={15} />
            {label}
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
