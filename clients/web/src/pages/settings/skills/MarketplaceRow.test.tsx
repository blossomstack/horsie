import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { MarketplaceView } from "../../../api/types";
import { answerConfirm, confirmSnapshot, resetConfirm } from "../../../lib/confirm";
import { MarketplaceRow } from "./MarketplaceRow";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);
afterEach(resetConfirm);

const install = vi.fn();
const refresh = vi.fn();
const remove = vi.fn();

vi.mock("../../../hooks/usePlugins", () => ({
  useInstallPlugin: () => ({ mutate: install, isPending: false }),
  useRefreshMarketplace: () => ({ mutate: refresh, isPending: false }),
  useRemoveMarketplace: () => ({ mutate: remove, isPending: false }),
}));

beforeEach(() => {
  install.mockClear();
  remove.mockClear();
});

function market(over: Partial<MarketplaceView> = {}): MarketplaceView {
  return {
    name: "official",
    sourceUrl: "https://example.com/market.git",
    sourceRef: undefined,
    pluginCount: 3,
    updatedAt: "1",
    skipped: [],
    plugins: [
      {
        name: "alpha",
        description: "the first",
        version: "1.0",
        installed: false,
      },
      { name: "beta", description: undefined, version: undefined, installed: true },
      {
        name: "gamma",
        description: undefined,
        version: undefined,
        installed: false,
      },
    ],
    ...over,
  };
}

describe("MarketplaceRow", () => {
  it("offers uninstalled entries and marks the installed one", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    expect(screen.getAllByTestId("marketplace-entry")).toHaveLength(3);
    // No jest-dom in this suite, so `disabled` is read off the element.
    const beta = screen.getByTestId("entry-install-beta") as HTMLButtonElement;
    const alpha = screen.getByTestId("entry-install-alpha") as HTMLButtonElement;
    expect(beta.disabled).toBe(true);
    expect(beta.textContent).toContain("Installed");
    expect(alpha.disabled).toBe(false);
  });

  // Installing by (marketplace, name) rather than by URL is what lets the
  // server resolve through the cached index instead of re-cloning.
  it("installs an entry by (marketplace, name), never by URL", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    fireEvent.click(screen.getByTestId("entry-install-alpha"));
    expect(install).toHaveBeenCalledWith({
      marketplace: "official",
      pluginName: "alpha",
    });
  });

  // The official catalogue has ~276 entries; the filter is why the list is
  // usable at all.
  it("narrows the list as you filter, over names and descriptions", () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    fireEvent.change(screen.getByTestId("marketplace-filter"), {
      target: { value: "gam" },
    });
    expect(screen.getAllByTestId("marketplace-entry")).toHaveLength(1);

    fireEvent.change(screen.getByTestId("marketplace-filter"), {
      target: { value: "the first" },
    });
    const only = screen.getAllByTestId("marketplace-entry");
    expect(only).toHaveLength(1);
    expect(only[0].textContent).toContain("alpha");
  });

  // A catalogue that quietly lost three plugins is a bug report nobody files.
  it("names entries it could not parse", () => {
    render(
      <MarketplaceRow
        marketplace={market({ skipped: ["entry 4: missing 'source'"] })}
        expanded
        onToggle={() => {}}
      />,
    );
    expect(screen.getByTestId("marketplace-skipped").textContent).toContain(
      "missing 'source'",
    );
  });

  it("collapses its entries when not expanded", () => {
    render(
      <MarketplaceRow marketplace={market()} expanded={false} onToggle={() => {}} />,
    );
    expect(screen.queryByTestId("marketplace-entry")).toBeNull();
    expect(screen.getByTestId("marketplace-row").textContent).toContain(
      "3 plugins",
    );
  });

  // Removing a source is not removing the software, and the confirm has to say
  // so — otherwise it reads like an uninstall.
  it("says installed bundles survive before removing a source", async () => {
    render(<MarketplaceRow marketplace={market()} expanded onToggle={() => {}} />);
    fireEvent.click(screen.getByLabelText("Remove marketplace"));
    expect(confirmSnapshot()?.message).toContain("stay installed");
    answerConfirm(true);
    await waitFor(() => expect(remove).toHaveBeenCalledWith("official"));
  });
});
