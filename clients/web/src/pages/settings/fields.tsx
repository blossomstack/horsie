import { Plus, Trash2 } from "lucide-react";
import { useId, type ReactNode } from "react";
import { useSettingsScroll } from "./scrollShadow";
import { SettingsHeader } from "./SettingsHeader";
import { cn } from "../../lib/cn";

/**
 * The whole shell every settings and admin page shares: the full-height
 * column, the header bar, and the scrolling body under it.
 *
 * `SettingsPane` alone was not enough. It was extracted because six pages
 * centred their content and Account did not — and then the SHELL around it
 * drifted the same way, because each page still assembled its own. Projects
 * put the header inside the pane and had no full-height wrapper at all, so
 * its pane stopped where the content did and the column's own ground showed
 * through underneath as a different colour for the bottom half of the page.
 *
 * A page cannot get that wrong if it does not write it. Take this, pass the
 * header's props through it, and put sections in the body.
 */
export function SettingsPage({
  title,
  desc,
  dirty,
  saved,
  saving,
  saveBlocked,
  onSave,
  onDiscard,
  children,
}: {
  title: string;
  desc: string;
  dirty?: boolean;
  saved?: boolean;
  saving?: boolean;
  saveBlocked?: boolean;
  onSave?: () => void;
  onDiscard?: () => void;
  children: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col overflow-hidden bg-chassis">
      <SettingsHeader
        title={title}
        desc={desc}
        dirty={dirty}
        saved={saved}
        saving={saving}
        saveBlocked={saveBlocked}
        onSave={onSave}
        onDiscard={onDiscard}
      />
      <SettingsPane>{children}</SettingsPane>
    </div>
  );
}

/**
 * A list of rows, separated by a hairline.
 *
 * Explicit rather than something `Section` does to whatever it is given.
 * While the section divided all its children, a group that held a list AND
 * the buttons under it drew a separator between the last row and the buttons
 * — a line across a boundary that is not one — and a single-row list still
 * got one under it.
 */
export function Rows({ children }: { children: ReactNode }) {
  return <div className="list-divided">{children}</div>;
}
function SettingsPane({ children }: { children: ReactNode }) {
  const { onScroll } = useSettingsScroll();
  return (
    <div className="min-h-0 flex-1 overflow-y-auto bg-chassis" onScroll={onScroll}>
      <div className="mx-auto max-w-3xl space-y-4 px-4 py-5 sm:px-6">
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
  addDisabled = false,
  addTitle,
  empty,
}: {
  title: string;
  desc: string;
  children: ReactNode;
  onAdd?: () => void;
  addLabel?: string;
  addTestId?: string;
  /** Adding is not possible yet. Say why in `addTitle` — and say it in `empty`
   * too, since a tooltip is not discoverable by everyone. */
  addDisabled?: boolean;
  addTitle?: string;
  empty?: string | null;
}) {
  return (
    <section className="section">
      {/* Wraps rather than overlapping: at 768px the Add key drew on top of
          the heading it was meant to sit beside. */}
      <div className="mb-2.5 flex flex-wrap items-start justify-between gap-x-4 gap-y-1.5">
        {/* Both strings can carry a name someone chose, and a provider named
            without spaces has nowhere to wrap: `break-words` is what keeps
            `Models · <long-name>` inside the panel. */}
        <div className="min-w-0">
          <h2 className="section-title break-words">{title}</h2>
          <p className="mt-1 max-w-prose text-xs leading-snug text-faint">
            {desc}
          </p>
        </div>
        {onAdd && (
          <button
            className="key shrink-0"
            onClick={onAdd}
            disabled={addDisabled}
            title={addTitle}
            data-testid={addTestId}
          >
            <Plus size={13} aria-hidden /> {addLabel}
          </button>
        )}
      </div>
      <div className="space-y-2">
        {empty && (
          <p className="screen break-words px-3 py-4 text-center text-sm text-faint">
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
        "-mx-1.5 px-1.5 transition-colors hover:bg-raised",
        active && "bg-accent-quiet",
      )}
      data-testid={testId}
      data-active={active ? "true" : undefined}
    >
      <div className="flex items-center gap-2 px-1 py-1.5">
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
      {children && <div className="px-1 pb-2.5">{children}</div>}
    </div>
  );
}

export function RowLabel({ children }: { children: ReactNode }) {
  return <span className="legend mb-0.5 block">{children}</span>;
}

/**
 * The note under a field: what it wants, or why it cannot be saved.
 *
 * Deliberately a sibling of the `<label>` rather than inside it. A wrapping
 * label contributes *all* its text to the control's accessible name, so a hint
 * placed inside would rename the field to "App ID The number on the app's page
 * on GitHub" for anyone listening. `aria-describedby` is the relationship this
 * actually is.
 */
function FieldNote({
  id,
  hint,
  invalid,
}: {
  id: string;
  hint?: ReactNode;
  invalid?: string | null;
}) {
  if (!invalid && !hint) return null;
  return (
    <span
      id={id}
      className={cn(
        "mt-1 block text-xs leading-relaxed",
        invalid ? "text-red-ink" : "text-dim",
      )}
    >
      {invalid ?? hint}
    </span>
  );
}

export function TextField({
  label,
  value,
  onChange,
  placeholder,
  type,
  hint,
  invalid,
  testId,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  type?: string;
  /** What the field wants, under it. A rule discovered only as a broken
   * session half an hour later is a rule the field should have stated. */
  hint?: ReactNode;
  /** Why this value cannot be saved. Replaces the hint while it holds. */
  invalid?: string | null;
  testId?: string;
}) {
  const noteId = useId();
  const described = invalid || hint ? noteId : undefined;
  return (
    <div className="block">
      <label className="block">
        <RowLabel>{label}</RowLabel>
        <input
          className={cn("field field-mono", invalid && "border-red")}
          type={type}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          aria-invalid={invalid ? true : undefined}
          aria-describedby={described}
          data-testid={testId}
        />
      </label>
      <FieldNote id={noteId} hint={hint} invalid={invalid} />
    </div>
  );
}

/** A multi-line field, for values that legitimately carry newlines. */
export function TextAreaField({
  label,
  value,
  onChange,
  placeholder,
  rows = 5,
  hint,
  invalid,
  testId,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  rows?: number;
  hint?: ReactNode;
  invalid?: string | null;
  testId?: string;
}) {
  const noteId = useId();
  const described = invalid || hint ? noteId : undefined;
  return (
    <div className="block">
      <label className="block">
        <RowLabel>{label}</RowLabel>
        <textarea
          className={cn("field field-mono", invalid && "border-red")}
          rows={rows}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          placeholder={placeholder}
          aria-invalid={invalid ? true : undefined}
          aria-describedby={described}
          data-testid={testId}
        />
      </label>
      <FieldNote id={noteId} hint={hint} invalid={invalid} />
    </div>
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
    <div className="-mx-1.5 bg-raised px-2.5 py-2.5">
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
