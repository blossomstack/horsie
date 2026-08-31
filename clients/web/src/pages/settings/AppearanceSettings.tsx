import { Check, Globe, Monitor, Moon, Sun } from "lucide-react";
import { useTranslation } from "react-i18next";
import { SETTINGS, useUiSettings } from "../../hooks/useUiSettings";
import {
  SKINS,
  TEXT_SIZES,
  useTheme,
  type Skin,
  type ThemeChoice,
} from "../../hooks/useTheme";
import {
  LOCALES,
  useLocaleChoice,
  type LocaleChoice,
} from "../../i18n";
import { cn } from "../../lib/cn";
import { Section, SettingsPage } from "./fields";

const MODES = [
  { id: "light", icon: Sun, labelKey: "appearance.modeLight" },
  { id: "dark", icon: Moon, labelKey: "appearance.modeDark" },
  { id: "system", icon: Monitor, labelKey: "appearance.modeSystem" },
] as const satisfies readonly {
  id: ThemeChoice;
  icon: typeof Sun;
  labelKey: string;
}[];

/**
 * A miniature of one world, drawn from that world's own tokens.
 *
 * `data-skin` and `data-theme` on the swatch itself make every value inside
 * resolve through that world's block — so the preview is the real palette
 * rather than a hand-copied approximation that can quietly go stale. It is
 * also why the seam defaults live in `@layer base` rather than behind a
 * higher-specificity `html[data-skin]`: this sets the attribute on a plain
 * `div`, and an `html`-anchored selector would never match it.
 */
function SkinSwatch({ skin, mode }: { skin: Skin; mode: "light" | "dark" }) {
  return (
    <div
      data-skin={skin === "paper" ? undefined : skin}
      data-theme={mode}
      className="pointer-events-none flex h-[4.25rem] w-full flex-col gap-1.5 overflow-hidden rounded-[var(--radius-control)] p-2"
      style={{ background: "var(--chassis)" }}
      aria-hidden
    >
      <div
        className="flex flex-1 items-center gap-1.5 rounded-[var(--radius-control)] px-1.5"
        style={{ background: "var(--panel)" }}
      >
        <span className="lamp" style={{ color: "var(--lamp-ok)" }} />
        <span
          className="h-1 flex-1 rounded-[999px]"
          style={{ background: "var(--legend-dim)", opacity: 0.5 }}
        />
      </div>
      <div className="flex items-center gap-1.5">
        <span
          className="h-4 w-9 rounded-[var(--radius-cap)]"
          style={{ background: "var(--accent)" }}
        />
        <span
          className="h-4 w-6 rounded-[var(--radius-cap)]"
          style={{ background: "var(--keycap)" }}
        />
        <span
          className="h-4 flex-1 rounded-[var(--radius-control)]"
          style={{ background: "var(--screen)" }}
        />
      </div>
    </div>
  );
}

/** How the interface looks, what language it speaks, and what it shows. All
 * three are per-browser choices — the server has no opinion about any. */
export function AppearanceSettings() {
  const { t } = useTranslation();
  const { choice, mode, skin, textSize, setChoice, setSkin, setTextSize } =
    useTheme();
  const { choice: localeChoice, locale, setChoice: setLocale } =
    useLocaleChoice();
  const { values, toggle } = useUiSettings();

  // `system` first: it is the default, and it is the entry whose current
  // resolution is worth showing next to it.
  const localeOptions: { id: LocaleChoice; name: string; note: string }[] = [
    {
      id: "system",
      name: t("appearance.languageSystem"),
      note: t("appearance.languageSystemNote"),
    },
    ...LOCALES,
  ];

  return (
    <SettingsPage title={t("appearance.title")}>
        <Section
          title={t("appearance.themeTitle")}
        >
          <div
            className="grid grid-cols-1 gap-2.5 sm:grid-cols-2"
            role="radiogroup"
            aria-label={t("appearance.themeGroup")}
          >
            {SKINS.map((s) => (
              <button
                key={s}
                type="button"
                role="radio"
                aria-checked={skin === s}
                onClick={() => setSkin(s)}
                data-testid={`skin-option-${s}`}
                className={cn(
                  "flex flex-col gap-2 rounded-[var(--radius-control)] p-2.5 text-left transition-colors",
                  // Selection is a fill and a tick, like every other selected
                  // thing in the build — not a ring around the card.
                  skin === s ? "bg-raised" : "hover:bg-raised",
                )}
              >
                <SkinSwatch skin={s} mode={mode} />
                <span className="flex items-center gap-1.5">
                  <span className="text-[0.8125rem] font-semibold text-legend">
                    {t(`appearance.skin.${s}.name`)}
                  </span>
                  {skin === s && (
                    <Check size={13} className="text-accent" aria-hidden />
                  )}
                </span>
              </button>
            ))}
          </div>
        </Section>

        <Section
          title={t("appearance.modeTitle")}
        >
          <div
            className="flex flex-wrap gap-2"
            role="radiogroup"
            aria-label={t("appearance.modeGroup")}
          >
            {MODES.map((m) => (
              <button
                key={m.id}
                type="button"
                role="radio"
                aria-checked={choice === m.id}
                onClick={() => setChoice(m.id)}
                data-testid={`mode-option-${m.id}`}
                className={cn("key", choice === m.id ? "key-go" : "key-blank")}
              >
                <m.icon size={13} aria-hidden />
                {t(m.labelKey)}
                {m.id === "system" && choice === "system" && (
                  <span className="opacity-70">
                    ·{" "}
                    {mode === "light"
                      ? t("appearance.modeLight")
                      : t("appearance.modeDark")}
                  </span>
                )}
              </button>
            ))}
          </div>
        </Section>

        <Section
          title={t("appearance.languageTitle")}
        >
          <div
            className="flex flex-wrap gap-2"
            role="radiogroup"
            aria-label={t("appearance.languageGroup")}
          >
            {localeOptions.map((option) => (
              <button
                key={option.id}
                type="button"
                role="radio"
                aria-checked={localeChoice === option.id}
                onClick={() => setLocale(option.id)}
                data-testid={`locale-option-${option.id}`}
                title={option.note}
                className={cn(
                  "key",
                  localeChoice === option.id ? "key-go" : "key-blank",
                )}
              >
                {option.id === "system" && <Globe size={13} aria-hidden />}
                {option.name}
                {option.id === "system" && localeChoice === "system" && (
                  <span className="opacity-70">
                    · {LOCALES.find((l) => l.id === locale)?.name}
                  </span>
                )}
              </button>
            ))}
          </div>
        </Section>

        <Section
          title={t("appearance.textSizeTitle")}
        >
          <div
            className="flex flex-wrap gap-2"
            role="radiogroup"
            aria-label={t("appearance.textSizeGroup")}
          >
            {TEXT_SIZES.map((size) => (
              <button
                key={size}
                type="button"
                role="radio"
                aria-checked={textSize === size}
                onClick={() => setTextSize(size)}
                data-testid={`text-size-option-${size}`}
                title={t(`appearance.textSize.${size}.blurb`)}
                className={cn("key", textSize === size ? "key-go" : "key-blank")}
              >
                {t(`appearance.textSize.${size}.name`)}
              </button>
            ))}
          </div>
        </Section>

        <Section
          title={t("appearance.transcriptTitle")}
        >
          {SETTINGS.map((def) => (
            <button
              key={def.key}
              type="button"
              role="switch"
              aria-checked={values[def.key]}
              onClick={() => toggle(def.key)}
              data-testid="setting-toggle"
              data-key={def.key}
              data-checked={values[def.key]}
              className="flex w-full items-start gap-2.5 rounded-[var(--radius-control)] bg-raised px-3 py-2.5 text-left shadow-[inset_0_0_0_1px_var(--rule)] transition-colors hover:bg-raised"
            >
              <span
                aria-hidden
                className={cn(
                  "mt-px flex h-4 w-4 shrink-0 items-center justify-center rounded-[3px]",
                  values[def.key]
                    ? "bg-accent text-accent-ink"
                    : "shadow-[inset_0_0_0_1px_var(--rule-strong)]",
                )}
              >
                {values[def.key] && <Check size={11} strokeWidth={3} />}
              </span>
              <span className="min-w-0 text-[0.8125rem] text-legend">
                {t(`ui.${def.key}.label`)}
              </span>
            </button>
          ))}
        </Section>
      </SettingsPage>
  );
}
