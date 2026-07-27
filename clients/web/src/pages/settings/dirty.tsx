import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  type ReactNode,
} from "react";

type DirtyCtx = {
  /** Called by pages that batch edits; a ref keeps the context value stable. */
  setDirty: (dirty: boolean) => void;
  /** True to proceed with navigation; prompts when there are unsaved edits. */
  confirmLeave: () => boolean;
};

const Ctx = createContext<DirtyCtx | null>(null);

export function SettingsDirtyProvider({ children }: { children: ReactNode }) {
  const dirtyRef = useRef(false);
  const value = useMemo<DirtyCtx>(
    () => ({
      setDirty: (d) => {
        dirtyRef.current = d;
      },
      confirmLeave: () =>
        !dirtyRef.current || window.confirm("Discard unsaved changes?"),
    }),
    [],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

/** Nav links use this to gate navigation. Safe to call outside the provider. */
export function useSettingsDirty(): DirtyCtx {
  return useContext(Ctx) ?? { setDirty: () => {}, confirmLeave: () => true };
}

/** Pages with a batched save publish their dirty flag and clear it on unmount. */
export function usePublishDirty(dirty: boolean) {
  const { setDirty } = useSettingsDirty();
  useEffect(() => {
    setDirty(dirty);
    return () => setDirty(false);
  }, [dirty, setDirty]);
}
