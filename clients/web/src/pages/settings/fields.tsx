import { Plus, Trash2 } from "lucide-react";
import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

/**
 * The scrolling body every settings and admin page shares.
 *
 * Extracted because it had already drifted: six pages centred their content in
 * a `max-w-3xl` column and Account did not, so the same app looked like two
 * apps depending on which item of the nav you clicked. One wrapper means the
 * next page cannot get it wrong.
 */
export function SettingsPane({ children }: { children: ReactNode }) {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl space-y-6 px-4 py-6 sm:px-6">
        {children}
      </div>
    </div>
  );
}

/** A titled block of the settings panel: what it configures, what it holds,
 * and one control to add to it. */
export function Section({
  title,
  desc,
  children,
  onAdd,
  addLabel,
  addTestId,
  empty,
}: {
  title: string;
  desc: string;
  children: ReactNode;
  onAdd?: () => void;
  addLabel?: string;
  addTestId?: string;
  empty?: string | null;
}) {
  return (
    <section className="panel p-4">
      <div className="mb-4 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h2 className="section-title">{title}</h2>
          <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
            {desc}
          </p>
        </div>
        {onAdd && (
          <button className="key shrink-0" onClick={onAdd} data-testid={addTestId}>
            <Plus size={13} aria-hidden /> {addLabel}
          </button>
        )}
      </div>
      <div className="space-y-2.5">
        {empty && (
          <p className="screen px-3 py-5 text-center text-sm text-faint">
            {empty}
          </p>
        )}
        {children}
      </div>
    </section>
  );
}

/** A per-row action. Icon-only with its word in the tooltip and the accessible
 * name, so a row of four of them stays a row rather than a paragraph. */
export function RowAction({
  icon,
  label,
  onClick,
  danger = false,
  disabled = false,
  pressed,
  testId,
}: {
  icon: ReactNode;
  label: string;
  onClick: () => void;
  danger?: boolean;
  disabled?: boolean;
  pressed?: boolean;
  testId?: string;
}) {
  return (
    <button
      type="button"
      className={cn(
        "key-icon !h-7 !w-7 shrink-0",
        danger && "hover:!bg-red-quiet hover:!text-red-ink",
        pressed && "bg-raised text-legend",
      )}
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      aria-pressed={pressed}
      title={label}
      data-testid={testId}
    >
      {icon}
    </button>
  );
}

/**
 * One entry in a list, as an identity line plus its actions — with the editor
 * folded away until asked for.
 *
 * The settings pages used to render every row as a fully-expanded form, so a
 * catalog of twenty model cards was twenty six-field forms stacked vertically
 * and there was no way to see what the catalog *contained* without scrolling
 * through all of it. A list should read as a list.
 */
export function ListRow({
  title,
  subtitle,
  meta,
  actions,
  children,
  onActivate,
  active = false,
  testId,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  /** Chips or lamps sitting between the identity and the actions. */
  meta?: ReactNode;
  actions?: ReactNode;
  /** The expanded body, rendered below the identity line when present. */
  children?: ReactNode;
  /** Makes the identity line itself a control — used where selecting the row
   * drives a detail pane. */
  onActivate?: () => void;
  active?: boolean;
  testId?: string;
}) {
  const identity = (
    <>
      <span className="min-w-0 flex-1">
        <span className="item-title block truncate">{title}</span>
        {subtitle && (
          <span className="mt-0.5 block truncate text-xs text-faint">
            {subtitle}
          </span>
        )}
      </span>
      {meta}
    </>
  );

  return (
    <div
      className={cn(
        "rounded-[var(--radius-control)] bg-raised shadow-[inset_0_0_0_1px_var(--row-ring)]",
        active && "shadow-[inset_0_0_0_1px_var(--rule-strong)]",
      )}
      data-testid={testId}
      data-active={active ? "true" : undefined}
    >
      <div className="flex items-center gap-2 px-3 py-2">
        {onActivate ? (
          <button
            type="button"
            className="flex min-w-0 flex-1 items-center gap-2 text-left"
            onClick={onActivate}
            aria-expanded={active}
          >
            {identity}
          </button>
        ) : (
          identity
        )}
        {actions && <span className="flex shrink-0 items-center gap-0.5">{actions}</span>}
      </div>
      {children && <div className="border-t px-3 py-3">{children}</div>}
    </div>
  );
}

export function RowLabel({ children }: { children: ReactNode }) {
  return <span className="legend mb-1 block">{children}</span>;
}

export function TextField({
  label,
  value,
  onChange,
  placeholder,
  type,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  type?: string;
}) {
  return (
    <label className="block">
      <RowLabel>{label}</RowLabel>
      <input
        className="field field-mono"
        type={type}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
      />
    </label>
  );
}

export function RowShell({
  onRemove,
  removeLabel,
  children,
}: {
  onRemove: () => void;
  removeLabel: string;
  children: ReactNode;
}) {
  return (
    <div className="rounded-[var(--radius-control)] bg-raised p-3 shadow-[inset_0_0_0_1px_var(--row-ring)]">
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">{children}</div>
        <button
          className="key-icon shrink-0 !h-7 !w-7 text-faint hover:!bg-red-quiet hover:!text-red-ink"
          onClick={onRemove}
          aria-label={removeLabel}
          title={removeLabel}
        >
          <Trash2 size={14} aria-hidden />
        </button>
      </div>
    </div>
  );
}
