import {
  Boxes,
  Brain,
  FolderTree,
  Cpu,
  Palette,
  Plug,
  SlidersHorizontal,
  UserCog,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";
import { SettingsDirtyProvider } from "./dirty";
import { SettingsScrollProvider } from "./scrollShadow";

/** The nav's words are looked up on render rather than frozen in a
 * module-level constant, which would keep whichever language the tab opened
 * in. */
const ITEMS = [
  { to: "projects", labelKey: "settingsNav.projects", icon: FolderTree },
  { to: "models", labelKey: "settingsNav.models", icon: SlidersHorizontal },
  { to: "runtimes", labelKey: "settingsNav.runtimes", icon: Cpu },
  { to: "skills", labelKey: "settingsNav.skills", icon: Boxes },
  { to: "memory", labelKey: "settingsNav.memory", icon: Brain },
  { to: "integrations", labelKey: "settingsNav.integrations", icon: Plug },
  { to: "appearance", labelKey: "settingsNav.appearance", icon: Palette },
  { to: "account", labelKey: "settingsNav.account", icon: UserCog },
] as const;

export function SettingsLayout() {
  const { t } = useTranslation();
  const items: NavItem[] = ITEMS.map(({ to, labelKey, icon }) => ({
    to,
    icon,
    label: t(labelKey),
  }));
  return (
    <SettingsDirtyProvider>
      <SettingsScrollProvider>
      <div className="flex h-full flex-col overflow-hidden md:flex-row">
        <SettingsNav title={t("nav.settings")} items={items} />
        <div className="min-w-0 flex-1">
          <Outlet />
        </div>
      </div>
      </SettingsScrollProvider>
    </SettingsDirtyProvider>
  );
}
