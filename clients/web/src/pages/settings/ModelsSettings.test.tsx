import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ProviderView, SettingsView } from "../../api/types";
import { ModelsSettings } from "./ModelsSettings";

afterEach(cleanup);

const provider = (over: Partial<ProviderView>): ProviderView => ({
  name: "p",
  kind: "chatgpt",
  baseUrl: undefined,
  hasCredential: false,
  keepThinkingSignature: false,
  ...over,
});

const settings: { current: SettingsView } = {
  current: undefined as unknown as SettingsView,
};

const view = (providers: ProviderView[]): SettingsView => ({
  providers,
  models: [],
  vendors: [],
  defaultVendor: "local",
  restartRequired: false,
  info: {
    configPath: "",
    database: "",
    stateDir: "",
    dataDir: "",
    pluginsDir: "",
    version: "test",
  },
});

vi.mock("../../hooks/useSettings", () => ({
  useRefreshSettings: () => vi.fn(),
  useSettings: () => ({
    data: settings.current,
    isLoading: false,
    isError: false,
  }),
  useUpdateSettings: () => ({
    mutateAsync: vi.fn(),
    isPending: false,
    isSuccess: false,
    isError: false,
    error: null,
  }),
}));

vi.mock("../../hooks/useModelCards", () => ({
  useModelCardSearch: () => ({ data: [] }),
}));

describe("ModelsSettings", () => {
  // A provider that cannot authenticate cannot serve a model, and the server
  // refuses the write anyway — so the button says no before the save does.
  it("will not add a model to a provider that has no credential", () => {
    settings.current = view([provider({ name: "codex" })]);
    render(<ModelsSettings />);

    const add = screen.getByRole("button", { name: /Add model/ });
    expect(add.hasAttribute("disabled")).toBe(true);
    // The reason is on the page, not only in a tooltip.
    expect(screen.getByText(/Connect “codex” to a ChatGPT plan first/)).toBeTruthy();
  });

  it("adds models once the plan is connected", () => {
    settings.current = view([provider({ name: "codex", hasCredential: true })]);
    render(<ModelsSettings />);

    expect(
      screen.getByRole("button", { name: /Add model/ }).hasAttribute("disabled"),
    ).toBe(false);
  });

  // The row is where you notice a provider is unusable, so it is also where
  // the fix lives — previously sign-in was reachable only by opening the
  // editor a second time, after the provider had already been saved.
  it("offers Connect on the row of an unconnected plan, and opens sign-in there", () => {
    settings.current = view([provider({ name: "codex" })]);
    render(<ModelsSettings />);

    expect(screen.getByText("Not connected")).toBeTruthy();
    expect(screen.queryByTestId("chatgpt-signin")).toBeNull();

    fireEvent.click(screen.getByTestId("provider-connect-codex"));

    expect(screen.getByTestId("chatgpt-signin")).toBeTruthy();
  });

  // A stored API key on a ChatGPT row is a leftover from a previous kind and
  // authorizes nothing; the lamp must not report it as a working provider.
  it("describes a key provider's credential in its own terms", () => {
    settings.current = view([
      provider({ name: "kimi", kind: "anthropic", hasCredential: true }),
    ]);
    render(<ModelsSettings />);

    expect(screen.getByText("Key set")).toBeTruthy();
    expect(screen.queryByTestId("provider-connect-kimi")).toBeNull();
  });
});
