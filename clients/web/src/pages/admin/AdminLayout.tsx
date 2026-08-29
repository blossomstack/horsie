import { GitBranch, Layers } from "lucide-react";
import { useTranslation } from "react-i18next";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";
import { SettingsDirtyProvider } from "../settings/dirty";

const ITEMS = [
  { to: "model-cards", labelKey: "adminNav.modelCards", icon: Layers },
  { to: "github-app", labelKey: "adminNav.githubApp", icon: GitBranch },
] as const;

/** Operator-facing surfaces. Adding a page = one more entry in ITEMS.
 *
 * Wrapped in the dirty provider because GitHub App is now the only page in the
 * product that batches its edits — settings moved to saving per item — and it
 * is the worst one to lose input on, since recovering means re-pasting a
 * private key. */
export function AdminLayout() {
  const { t } = useTranslation();
  const items: NavItem[] = ITEMS.map(({ to, labelKey, icon }) => ({
    to,
    icon,
    label: t(labelKey),
  }));
  return (
    <SettingsDirtyProvider>
      <div className="flex h-full flex-col overflow-hidden md:flex-row">
        <SettingsNav title={t("nav.admin")} items={items} />
        <div className="min-w-0 flex-1">
          <Outlet />
        </div>
      </div>
    </SettingsDirtyProvider>
  );
}
