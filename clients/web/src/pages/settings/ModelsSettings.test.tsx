import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ModelView, ProviderView, SettingsView } from "../../api/types";
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

const model = (over: Partial<ModelView>): ModelView => ({
  alias: "sonnet",
  provider: "p",
  modelId: "claude-sonnet-4-5",
  ...over,
});

const view = (providers: ProviderView[], models: ModelView[] = []): SettingsView => ({
  providers,
  models,
  vendors: [],
  defaultRuntimeVendor: "local",
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

/** A settings mutation that is idle and never resolves anything. */
const idleMutation = () => ({
  mutateAsync: vi.fn(),
  isPending: false,
  isSuccess: false,
  isError: false,
  error: null,
});

/** What the model form last saved, so a round trip can be read back. */
const savedModel = vi.fn();

vi.mock("../../hooks/useSettings", () => ({
  useRefreshSettings: () => vi.fn(),
  useSettings: () => ({
    data: settings.current,
    isLoading: false,
    isError: false,
  }),
  usePutProvider: () => idleMutation(),
  useDeleteProvider: () => idleMutation(),
  usePutModel: () => ({ ...idleMutation(), mutateAsync: savedModel }),
  useDeleteModel: () => idleMutation(),
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

  // Whether a model may be shown an attachment is a property of the model, and
  // the model form is the only place it is set — so it has to be visible there
  // and it has to survive a save.
  it("shows each vision flag as its own box, checked from the saved model", () => {
    settings.current = view(
      [provider({ name: "p", kind: "anthropic", hasCredential: true })],
      [model({ supportsImages: true })],
    );
    render(<ModelsSettings />);

    fireEvent.click(screen.getByTestId("model-edit-sonnet"));

    const images = screen.getByTestId("model-supports-images") as HTMLInputElement;
    const documents = screen.getByTestId("model-supports-documents") as HTMLInputElement;
    expect(images.checked).toBe(true);
    // Two flags, not one: a model that takes images need not take PDFs.
    expect(documents.checked).toBe(false);
  });

  it("sends both flags on save", async () => {
    savedModel.mockClear();
    settings.current = view(
      [provider({ name: "p", kind: "anthropic", hasCredential: true })],
      [model({})],
    );
    render(<ModelsSettings />);

    fireEvent.click(screen.getByTestId("model-edit-sonnet"));
    fireEvent.click(screen.getByTestId("model-supports-images"));
    fireEvent.click(screen.getByTestId("model-supports-documents"));
    fireEvent.click(screen.getByTestId("editor-save"));

    expect(savedModel).toHaveBeenCalledTimes(1);
    expect(savedModel.mock.calls[0][0]).toMatchObject({
      body: { supportsImages: true, supportsDocuments: true },
    });
  });

  // An unticked box means "does not take them", and has to be sent as false —
  // dropping it would leave a model that used to see still seeing.
  it("sends a cleared flag as false, not as absent", async () => {
    savedModel.mockClear();
    settings.current = view(
      [provider({ name: "p", kind: "anthropic", hasCredential: true })],
      [model({ supportsImages: true })],
    );
    render(<ModelsSettings />);

    fireEvent.click(screen.getByTestId("model-edit-sonnet"));
    fireEvent.click(screen.getByTestId("model-supports-images"));
    fireEvent.click(screen.getByTestId("editor-save"));

    expect(savedModel.mock.calls[0][0]).toMatchObject({
      body: { supportsImages: false, supportsDocuments: false },
    });
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
