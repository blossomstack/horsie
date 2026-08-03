import { GitBranch, Layers } from "lucide-react";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";

const ITEMS: NavItem[] = [
  { to: "model-cards", label: "Model cards", icon: Layers },
  { to: "github-app", label: "GitHub App", icon: GitBranch },
];

/** Operator-facing surfaces. Adding a page = one more entry in ITEMS. */
export function AdminLayout() {
  return (
    <div className="flex h-full flex-col overflow-hidden md:flex-row">
      <SettingsNav title="Admin" items={ITEMS} />
      <div className="min-w-0 flex-1">
        <Outlet />
      </div>
    </div>
  );
}
