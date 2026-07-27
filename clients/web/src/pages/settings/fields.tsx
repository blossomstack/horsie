import { Plus, Trash2 } from "lucide-react";
import type { ReactNode } from "react";

/** A titled card of rows with an "add" affordance and an empty-state note. */
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
    <section className="card p-4">
      <div className="mb-3 flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-text">{title}</h2>
          <p className="mt-0.5 text-xs text-faint">{desc}</p>
        </div>
        <button className="btn-outline shrink-0 !px-2.5 !py-1.5 text-xs" onClick={onAdd}>
          <Plus size={14} /> {addLabel}
        </button>
      </div>
      <div className="space-y-2.5">
        {empty && (
          <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
            {empty}
          </p>
        )}
        {children}
      </div>
    </section>
  );
}

export function RowLabel({ children }: { children: ReactNode }) {
  return (
    <span className="mb-1 block text-[11px] font-semibold text-muted">
      {children}
    </span>
  );
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
        className="input font-mono"
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
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
    >
      <div className="flex items-start gap-3">
        <div className="min-w-0 flex-1">{children}</div>
        <button
          className="btn-icon shrink-0 text-faint hover:text-error"
          onClick={onRemove}
          aria-label={removeLabel}
        >
          <Trash2 size={15} />
        </button>
      </div>
    </div>
  );
}
