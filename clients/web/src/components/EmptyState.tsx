import type { ReactNode } from "react";

export function EmptyState({
  icon,
  title,
  children,
}: {
  icon: ReactNode;
  title: string;
  children?: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
      <div className="flex h-14 w-14 items-center justify-center rounded-2xl border bg-surface-2 text-faint">
        {icon}
      </div>
      <h2 className="text-base font-semibold text-text">{title}</h2>
      {children && (
        <p className="max-w-sm text-sm text-muted">{children}</p>
      )}
    </div>
  );
}
