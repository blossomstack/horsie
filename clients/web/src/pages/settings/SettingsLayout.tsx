import {
  Boxes,
  Brain,
  Cpu,
  Plug,
  SlidersHorizontal,
  UserCog,
} from "lucide-react";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";
import { SettingsDirtyProvider } from "./dirty";

const ITEMS: NavItem[] = [
  { to: "models", label: "Models", icon: SlidersHorizontal },
  { to: "runtimes", label: "Runtimes", icon: Cpu },
  { to: "skills", label: "Skills", icon: Boxes },
  { to: "memory", label: "Memory", icon: Brain },
  { to: "integrations", label: "Integrations", icon: Plug },
  { to: "account", label: "Account", icon: UserCog },
];

export function SettingsLayout() {
  return (
    <SettingsDirtyProvider>
      <div className="flex h-full overflow-hidden">
        <SettingsNav title="Settings" items={ITEMS} />
        <div className="min-w-0 flex-1">
          <Outlet />
        </div>
      </div>
    </SettingsDirtyProvider>
  );
}
