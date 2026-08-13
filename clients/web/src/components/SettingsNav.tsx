import type { LucideIcon } from "lucide-react";
import { NavLink } from "react-router-dom";
import { cn } from "../lib/cn";
import { useSettingsDirty } from "../pages/settings/dirty";

export type NavItem = { to: string; label: string; icon: LucideIcon };

/** The page switcher for the settings/admin areas: a column beside the content
 * on a desk, a scrolling strip of keys above it on a phone. */
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
      className="flex shrink-0 flex-col border-rule bg-chassis md:h-full md:w-48 md:border-r"
      data-testid="settings-nav"
      aria-label={title}
    >
      <p className="legend hidden px-4 pb-2 pt-4 md:block">{title}</p>
      <div className="flex gap-1 overflow-x-auto border-b px-2 py-2 [mask-image:linear-gradient(to_right,black_calc(100%-2rem),transparent)] md:mask-none md:flex-col md:gap-px md:overflow-visible md:border-b-0 md:py-0">
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
                "flex shrink-0 items-center gap-2 rounded-[var(--radius-control)] px-2.5 py-2 text-[0.8125rem] transition-colors md:gap-2.5",
                // Fill only, like every other selected row in the app.
                isActive
                  ? "bg-raised text-legend"
                  : "text-dim hover:bg-raised hover:text-legend",
              )
            }
          >
            <Icon size={14} aria-hidden />
            {label}
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
