import type { LucideIcon } from "lucide-react";
import { NavLink } from "react-router-dom";
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
      className="flex shrink-0 flex-col bg-chassis max-md:bar-scroll md:h-full md:w-48 md:column-edge-r"
      data-testid="settings-nav"
      aria-label={title}
    >
      <p className="legend hidden px-4 pb-2 pt-4 md:block">{title}</p>
      <div className="flex gap-1 overflow-x-auto px-2 py-2 [mask-image:linear-gradient(to_right,black_calc(100%-2rem),transparent)] md:mask-none md:flex-col md:gap-px md:overflow-visible md:md:py-0">
        {items.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
            end
            data-testid={`settings-nav-${to}`}
            onClick={(e) => {
              if (!confirmLeave()) e.preventDefault();
            }}
            className="row row-quiet shrink-0 gap-2 px-2.5 py-2 text-[0.8125rem] md:gap-2.5"
          >
            <Icon size={14} aria-hidden />
            {label}
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
