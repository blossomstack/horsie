import { Layers } from "lucide-react";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";

const ITEMS: NavItem[] = [
  { to: "model-cards", label: "Model cards", icon: Layers },
];

/** Operator-facing surfaces. Adding a page = one more entry in ITEMS. */
export function AdminLayout() {
  return (
    <div className="flex h-full overflow-hidden">
      <SettingsNav title="Admin" items={ITEMS} />
      <div className="min-w-0 flex-1">
        <Outlet />
      </div>
    </div>
  );
}
