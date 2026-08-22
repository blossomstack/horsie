import { createContext, useContext, type ReactNode, type UIEvent } from "react";
import { useScrolledUnder } from "../../hooks/useScrolledUnder";

/**
 * Whether the settings pane has content scrolled up under its header.
 *
 * A context for one boolean, which needs justifying. `SettingsHeader` and
 * `SettingsPane` are siblings on every settings and admin page, but they are
 * separate components, so the state has to cross between them — and the pages
 * that render both are ten and counting. Threading a hook through each is ten
 * chances to forget, and a page that forgets shows no defect until someone
 * scrolls it.
 *
 * Provided once by `SettingsLayout`, so a new settings page gets the
 * behaviour by using the same two components everything else uses.
 */
const Ctx = createContext<{
  onScroll: (e: UIEvent<HTMLElement>) => void;
  barProps: { "data-scrolled"?: "true" };
}>({ onScroll: () => {}, barProps: {} });

export function SettingsScrollProvider({ children }: { children: ReactNode }) {
  const { onScroll, barProps } = useScrolledUnder();
  return <Ctx.Provider value={{ onScroll, barProps }}>{children}</Ctx.Provider>;
}

export const useSettingsScroll = () => useContext(Ctx);
