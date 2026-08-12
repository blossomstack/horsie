import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiRequestError } from "../../api/client";
import type {
  RuntimeVendorConfigView,
  SettingsView,
  VendorView,
} from "../../api/types";
import { answerConfirm, confirmSnapshot, resetConfirm } from "../../lib/confirm";
import { RuntimesSettings } from "./RuntimesSettings";
import { withConnectPath } from "./VendorForm";

afterEach(cleanup);
afterEach(resetConfirm);

const saved = vi.fn();
const removed = vi.fn();
const tested = vi.fn();
const defaulted = vi.fn();
const vendors: { current: RuntimeVendorConfigView[] } = { current: [] };
const roster: { current: VendorView[] } = { current: [] };
const readFailed: { current: unknown } = { current: null };

beforeEach(() => {
  readFailed.current = null;
  vendors.current = [];
  roster.current = [];
  vi.clearAllMocks();
});

const vendor = (): RuntimeVendorConfigView => ({
  name: "fly",
  settings: {
    kind: "Fly",
    value: {
      app: "horsie-runtimes",
      image: "ghcr.io/o/runtime:1",
      region: "iad",
      workspaceRoot: "/workspaces",
      callbackUrl: "wss://horsie.example.com/api/runtime/connect",
      volumes: true,
      cpuKind: "shared",
      cpus: 1,
      memoryMb: 1024,
      volumeSizeGb: 10,
    },
  },
  hasCredential: true,
  createdAt: "1",
  updatedAt: "1",
});

const listed = (name: string, isDefault = false): VendorView => ({
  name,
  isDefault,
  capabilities: { supportsProvisioning: true },
});

const settings = (): SettingsView => ({
  providers: [],
  models: [],
  vendors: roster.current,
  defaultRuntimeVendor: "",
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
  useSettings: () => ({ data: settings(), isLoading: false, error: null }),
  useSetDefaultRuntimeVendor: () => ({
    mutateAsync: defaulted,
    isPending: false,
    isSuccess: false,
  }),
}));

vi.mock("../../hooks/useRuntimeVendors", () => ({
  useRuntimeVendors: () => ({
    data: readFailed.current ? undefined : vendors.current,
    isLoading: false,
    isError: !!readFailed.current,
    error: readFailed.current,
  }),
  useSaveRuntimeVendor: () => ({ mutateAsync: saved, isPending: false }),
  useDeleteRuntimeVendor: () => ({ mutate: removed, isPending: false }),
  useTestRuntimeVendor: () => ({ mutateAsync: tested, isPending: false }),
}));

const field = (label: string) =>
  screen.getByText(label).parentElement!.querySelector("input")!;

describe("RuntimesSettings", () => {
  // The defect this page was rebuilt for: a cloud vendor is in the live roster
  // *and* in the cloud configuration, and used to be drawn once from each.
  it("draws a cloud vendor once, not once per source", () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    render(<RuntimesSettings />);
    expect(screen.getAllByTestId("vendor-row-fly")).toHaveLength(1);
    // The second draw carried its own testid, so counting one kind of row
    // would have passed against the two-list page it replaces.
    expect(screen.queryByTestId("cloud-vendor-row-fly")).toBeNull();
    expect(screen.getAllByText("fly")).toHaveLength(1);
    expect(screen.getByText(/Fly · iad/)).toBeTruthy();
  });

  it("gives a dialled-in agent no cloud actions", () => {
    roster.current = [listed("laptop")];
    render(<RuntimesSettings />);
    expect(screen.getByTestId("vendor-row-laptop")).toBeTruthy();
    expect(screen.getByText("Connected")).toBeTruthy();
    expect(screen.queryByTestId("cloud-vendor-edit-laptop")).toBeNull();
    expect(screen.queryByTestId("cloud-vendor-delete-laptop")).toBeNull();
  });

  it("lists a configured vendor without exposing its token", () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    const { container } = render(<RuntimesSettings />);
    expect(screen.getByText("Token set")).toBeTruthy();
    // The row reports that a token exists and offers no way to see it; the
    // token input only appears once an edit is opened.
    expect(container.querySelector("input[type=password]")).toBeNull();
  });

  // Editing used to render the form at the foot of the page, past every other
  // row — so the row you clicked and the form you got were nowhere near each
  // other.
  it("opens the editor inside the row it belongs to", () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly"), listed("laptop")];
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-edit-fly"));
    const row = screen.getByTestId("vendor-row-fly");
    expect(row.contains(screen.getByTestId("cloud-vendor-save"))).toBe(true);
  });

  it("deletes from the row, and only once confirmed", async () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-delete-fly"));
    expect(removed).not.toHaveBeenCalled();
    expect(confirmSnapshot()?.message).toContain("fly");

    answerConfirm(true);
    await waitFor(() => expect(removed).toHaveBeenCalledWith("fly", expect.anything()));
  });

  it("leaves the vendor alone when the confirm is cancelled", async () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-delete-fly"));
    answerConfirm(false);
    await waitFor(() => expect(confirmSnapshot()).toBeNull());
    expect(removed).not.toHaveBeenCalled();
  });

  // A dead read used to reach the empty branch through `vendors?.length ?? 0`,
  // so an unreachable server and an account that never configured a vendor
  // rendered the same sentence — and the one it rendered was the false one.
  it("reports a failed read instead of claiming there are none", () => {
    readFailed.current = new ApiRequestError(
      0,
      "network",
      "Could not reach the horsie server. Is `horsie serve` running?",
    );
    render(<RuntimesSettings />);
    expect(screen.getByTestId("cloud-vendors-error").textContent).toContain(
      "Couldn’t load cloud vendors",
    );
  });

  it("says so when there genuinely are none", () => {
    render(<RuntimesSettings />);
    expect(screen.queryByTestId("cloud-vendors-error")).toBeNull();
    expect(screen.getByText(/No runtimes yet/)).toBeTruthy();
  });

  it("saves a new fly vendor as an adjacently tagged union", async () => {
    // The wire shape is `{kind, value}`, not a flat object with a kind field:
    // a flat body deserialises to nothing and fails as a 422.
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add-fly"));

    fireEvent.change(field("Name"), { target: { value: "fly" } });
    fireEvent.change(field("Fly app"), { target: { value: "horsie-runtimes" } });
    fireEvent.change(field("API token"), { target: { value: "tok" } });
    fireEvent.change(field("Runtime image"), {
      target: { value: "ghcr.io/o/runtime:1" },
    });
    fireEvent.change(field("Callback URL"), {
      target: { value: "wss://horsie.example.com" },
    });
    fireEvent.click(screen.getByTestId("cloud-vendor-save"));

    await waitFor(() => expect(saved).toHaveBeenCalledTimes(1));
    expect(saved.mock.calls[0][0]).toEqual({
      name: "fly",
      body: {
        name: "fly",
        settings: {
          kind: "Fly",
          value: expect.objectContaining({
            app: "horsie-runtimes",
            image: "ghcr.io/o/runtime:1",
            // Typed as a bare origin, sent complete: the server refuses a
            // callback url with no path, and completing one is a typing
            // affordance that belongs here rather than in the API.
            callbackUrl: "wss://horsie.example.com/api/runtime/connect",
          }),
        },
        credential: "tok",
      },
    });
  });

  it("saves a velos vendor with velos-shaped settings", async () => {
    // The union is what stops a client describing a vendor with the wrong
    // substrate's fields — velos has a server URL and no region.
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add-velos"));

    expect(screen.queryByText("Region")).toBeNull();
    fireEvent.change(field("Name"), { target: { value: "velos" } });
    fireEvent.change(field("velos server URL"), {
      target: { value: "http://velos:8080" },
    });
    fireEvent.change(field("Runtime image"), {
      target: { value: "ghcr.io/o/runtime:1" },
    });
    fireEvent.change(field("Callback URL"), {
      target: { value: "ws://horsie.internal:3789" },
    });
    fireEvent.click(screen.getByTestId("cloud-vendor-save"));

    await waitFor(() => expect(saved).toHaveBeenCalledTimes(1));
    const settings = saved.mock.calls[0][0].body.settings;
    expect(settings.kind).toBe("Velos");
    expect(settings.value.serverUrl).toBe("http://velos:8080");
    expect(settings.value).not.toHaveProperty("region");
  });

  it("refuses to add a vendor under a name already taken", async () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add-fly"));
    fireEvent.change(field("Name"), { target: { value: "fly" } });
    fireEvent.click(screen.getByTestId("cloud-vendor-save"));

    expect(screen.getByTestId("cloud-vendor-error").textContent).toContain(
      "already exists",
    );
    expect(saved).not.toHaveBeenCalled();
  });

  it("reports a vendor that answers", async () => {
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    tested.mockResolvedValue({ ok: true });
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-test-fly"));

    await waitFor(() => expect(screen.getByTestId("cloud-vendor-ok-fly")).toBeTruthy());
    expect(tested).toHaveBeenCalledWith("fly");
  });

  it("shows what the substrate said when a check fails", async () => {
    // The failure this whole path exists for: a token that was good when it was
    // saved and is not any more. The row has to say so in the substrate's own
    // words — "unreachable" alone sends someone to check their network.
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    tested.mockResolvedValue({ ok: false, error: "fly refused: 401: unauthorized" });
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-test-fly"));

    await waitFor(() =>
      expect(
        screen.getByTestId("cloud-vendor-test-error-fly").textContent,
      ).toContain("401"),
    );
    expect(screen.queryByTestId("cloud-vendor-ok-fly")).toBeNull();
  });

  it("omits the credential when an edit leaves it blank", async () => {
    // The token cannot be read back, so re-typing it to change a region would
    // be the only way to edit anything — and forgetting to would wipe it.
    vendors.current = [vendor()];
    roster.current = [listed("fly")];
    render(<RuntimesSettings />);
    fireEvent.click(screen.getByTestId("cloud-vendor-edit-fly"));
    fireEvent.change(field("Region"), { target: { value: "lhr" } });
    fireEvent.click(screen.getByTestId("cloud-vendor-save"));

    await waitFor(() => expect(saved).toHaveBeenCalledTimes(1));
    const body = saved.mock.calls[0][0].body;
    expect(body.credential).toBeUndefined();
    expect(body.settings.value.region).toBe("lhr");
  });
});

describe("withConnectPath", () => {
  it("completes a bare origin", () => {
    expect(withConnectPath("wss://horsie.example.com")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("does not double up a trailing slash", () => {
    // The completion used to live server-side, where a stray keystroke produced
    // `//api/runtime/connect` — a path axum will not route, so every runtime
    // dialled a 404 and the vendor looked broken rather than mistyped.
    expect(withConnectPath("wss://horsie.example.com/")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("leaves an explicit path alone", () => {
    expect(withConnectPath("wss://horsie.example.com/relay/rt")).toBe(
      "wss://horsie.example.com/relay/rt",
    );
  });

  it("trims what was pasted", () => {
    expect(withConnectPath("  wss://horsie.example.com  ")).toBe(
      "wss://horsie.example.com/api/runtime/connect",
    );
  });

  it("passes through what it cannot parse", () => {
    // The server refuses this with a message written for a human. Guessing at
    // a scheme here would only turn that into a different error.
    expect(withConnectPath("horsie.example.com")).toBe("horsie.example.com");
  });
});
