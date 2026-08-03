import { Plus, Trash2 } from "lucide-react";
import type { ReactNode } from "react";

/** A titled block of the settings panel: what it configures, what it holds,
 * and one control to add to it. */
export function Section({
  title,
  desc,
  children,
  onAdd,
  addLabel,
  empty,
}: {
  title: string;
  desc: string;
  children: ReactNode;
  onAdd: () => void;
  addLabel: string;
  empty: string | null;
}) {
  return (
    <section className="panel p-4">
      <div className="mb-4 flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h2 className="font-mono text-[12px] font-semibold uppercase tracking-[0.1em] text-legend">
            {title}
          </h2>
          <p className="mt-1.5 max-w-prose text-xs leading-relaxed text-faint">
            {desc}
          </p>
        </div>
        <button className="key shrink-0" onClick={onAdd}>
          <Plus size={13} aria-hidden /> {addLabel}
        </button>
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
    <div className="rounded-[var(--radius-control)] bg-raised p-3 shadow-[inset_0_0_0_1px_var(--rule)]">
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
