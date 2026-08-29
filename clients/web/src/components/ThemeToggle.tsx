import { Moon, Sun } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useTheme } from "../hooks/useTheme";

/** The quick light/dark flip on the rail. Choosing a theme, and following the
 * system, live on the Appearance settings page — this is the one-click version
 * of the one axis people change often. */
export function ThemeToggle() {
  const { t } = useTranslation();
  const { mode, toggle } = useTheme();
  return (
    <button
      className="key-icon shrink-0"
      onClick={toggle}
      title={
        mode === "dark" ? t("themeToggle.toLight") : t("themeToggle.toDark")
      }
      aria-label={t("themeToggle.ariaLabel")}
      data-testid="theme-toggle"
    >
      {mode === "dark" ? <Sun size={14} /> : <Moon size={14} />}
    </button>
  );
}
