import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiRequestError } from "../../api/client";
import type { InstallOutcome, MarketplaceView } from "../../api/types";
import { SkillsSettings } from "./SkillsSettings";

afterEach(cleanup);

const CATALOGUE: MarketplaceView = {
  name: "catalogue",
  sourceUrl: "https://example.com/market.git",
  sourceRef: undefined,
  pluginCount: 2,
  updatedAt: "1",
  skipped: [],
  plugins: [
    { name: "alpha", description: undefined, version: undefined, installed: false },
    { name: "beta", description: undefined, version: undefined, installed: false },
  ],
};

const installOutcome: { current: InstallOutcome } = {
  current: { outcome: "Marketplace", value: CATALOGUE },
};

/** What `/api/plugins/marketplaces` answered: a list, or a failure. */
const marketplaces: { current: MarketplaceView[] } = { current: [CATALOGUE] };
const marketplacesFailed: { current: unknown } = { current: null };

beforeEach(() => {
  marketplaces.current = [CATALOGUE];
  marketplacesFailed.current = null;
});

vi.mock("../../hooks/usePlugins", () => ({
  usePlugins: () => ({ data: [], isLoading: false, isError: false }),
  useMarketplaces: () => ({
    data: marketplacesFailed.current ? undefined : marketplaces.current,
    isError: !!marketplacesFailed.current,
    error: marketplacesFailed.current,
  }),
  useInstallPlugin: () => ({
    mutateAsync: async () => installOutcome.current,
    isPending: false,
    isError: false,
    error: null,
    mutate: vi.fn(),
  }),
  useRefreshMarketplace: () => ({ mutate: vi.fn(), isPending: false }),
  useRemoveMarketplace: () => ({ mutate: vi.fn(), isPending: false }),
  useUpdatePlugin: () => ({ mutate: vi.fn(), isPending: false }),
  useSetPluginDefault: () => ({ mutate: vi.fn(), isPending: false }),
  useRemovePlugin: () => ({ mutate: vi.fn(), isPending: false }),
}));

describe("SkillsSettings", () => {
  // The section is hidden when there are no catalogues, and a failed read used
  // to take that same door out — so a server whose marketplaces could not be
  // read looked like a server that had never had one.
  it("keeps the Marketplaces section when its read failed", () => {
    marketplacesFailed.current = new ApiRequestError(
      0,
      "network",
      "Could not reach the horsie server. Is `horsie serve` running?",
    );
    render(<SkillsSettings />);
    expect(screen.getByRole("heading", { name: "Marketplaces" })).not.toBeNull();
    expect(screen.getByTestId("marketplaces-error").textContent).toContain(
      "Couldn’t load marketplaces",
    );
  });

  it("still hides it when the server genuinely has none", () => {
    marketplaces.current = [];
    render(<SkillsSettings />);
    expect(screen.queryByTestId("marketplaces-error")).toBeNull();
    expect(screen.queryByRole("heading", { name: "Marketplaces" })).toBeNull();
  });

  // Pasting a catalogue URL is not an error and not a dead end: the source it
  // registered opens, so the next click is the one the person came to make.
  it("opens the source a pasted catalogue URL registered", async () => {
    render(<SkillsSettings />);
    // Collapsed until the install box says otherwise.
    expect(screen.queryByTestId("marketplace-entry")).toBeNull();

    fireEvent.change(screen.getByLabelText("Git URL"), {
      target: { value: "https://example.com/market.git" },
    });
    fireEvent.click(screen.getByRole("button", { name: /Install/ }));

    await waitFor(() =>
      expect(screen.getAllByTestId("marketplace-entry")).toHaveLength(2),
    );
    expect(screen.getByTestId("entry-install-beta")).not.toBeNull();
  });
});
