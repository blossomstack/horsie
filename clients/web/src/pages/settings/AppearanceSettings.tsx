import { Check, Monitor, Moon, Sun } from "lucide-react";
import { SETTINGS, useUiSettings } from "../../hooks/useUiSettings";
import {
  SKINS,
  TEXT_SIZES,
  useTheme,
  type Skin,
  type ThemeChoice,
} from "../../hooks/useTheme";
import { cn } from "../../lib/cn";
import { Section, SettingsPage } from "./fields";

const MODES: { id: ThemeChoice; label: string; icon: typeof Sun }[] = [
  { id: "light", label: "Light", icon: Sun },
  { id: "dark", label: "Dark", icon: Moon },
  { id: "system", label: "System", icon: Monitor },
];

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

/** How the interface looks, and what it shows. Both are per-browser choices —
 * the server has no opinion about either. */
export function AppearanceSettings() {
  const { choice, mode, skin, textSize, setChoice, setSkin, setTextSize } =
    useTheme();
  const { values, toggle } = useUiSettings();

  return (
    <SettingsPage
        title="Appearance"
        desc="How this browser renders horsie. Stored locally, not on the server, so each browser you use can differ."
    >
        <Section
          title="Theme"
          desc="Same layouts, different material. Every theme ships light and dark, and every one is measured to WCAG AA in both."
        >
          <div
            className="grid grid-cols-1 gap-2.5 sm:grid-cols-2"
            role="radiogroup"
            aria-label="Theme"
          >
            {SKINS.map((s) => (
              <button
                key={s.id}
                type="button"
                role="radio"
                aria-checked={skin === s.id}
                onClick={() => setSkin(s.id)}
                data-testid={`skin-option-${s.id}`}
                className={cn(
                  "flex flex-col gap-2 rounded-[var(--radius-control)] p-2.5 text-left transition-colors",
                  // Selection is a fill and a tick, like every other selected
                  // thing in the build — not a ring around the card.
                  skin === s.id ? "bg-raised" : "hover:bg-raised",
                )}
              >
                <SkinSwatch skin={s.id} mode={mode} />
                <span className="flex items-center gap-1.5">
                  <span className="text-[0.8125rem] font-semibold text-legend">
                    {s.name}
                  </span>
                  {skin === s.id && (
                    <Check size={13} className="text-accent" aria-hidden />
                  )}
                </span>
                <span className="text-xs leading-snug text-faint">{s.blurb}</span>
              </button>
            ))}
          </div>
        </Section>

        <Section
          title="Light or dark"
          desc="System follows your operating system and keeps following it while this tab is open."
        >
          <div className="flex flex-wrap gap-2" role="radiogroup" aria-label="Mode">
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
                {m.label}
                {m.id === "system" && choice === "system" && (
                  <span className="opacity-70">· {mode}</span>
                )}
              </button>
            ))}
          </div>
        </Section>

        <Section
          title="Text size"
          desc="Scales every measurement in the interface, so the spacing grows with the type rather than the type outgrowing its slots."
        >
          <div
            className="flex flex-wrap gap-2"
            role="radiogroup"
            aria-label="Text size"
          >
            {TEXT_SIZES.map((t) => (
              <button
                key={t.id}
                type="button"
                role="radio"
                aria-checked={textSize === t.id}
                onClick={() => setTextSize(t.id)}
                data-testid={`text-size-option-${t.id}`}
                title={t.blurb}
                className={cn("key", textSize === t.id ? "key-go" : "key-blank")}
              >
                {t.name}
              </button>
            ))}
          </div>
        </Section>

        <Section
          title="Transcript"
          desc="What the session view shows. These are display switches, not session settings — they change nothing about how the agent runs."
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
              <span className="min-w-0">
                <span className="block text-[0.8125rem] text-legend">{def.label}</span>
                <span className="mt-0.5 block text-xs leading-snug text-faint">
                  {def.description}
                </span>
              </span>
            </button>
          ))}
        </Section>
      </SettingsPage>
  );
}
