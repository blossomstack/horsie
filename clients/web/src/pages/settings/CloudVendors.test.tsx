import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiRequestError } from "../../api/client";
import type { RuntimeVendorConfigView } from "../../api/types";
import { CloudVendors, withConnectPath } from "./CloudVendors";

afterEach(cleanup);

const saved = vi.fn();
const removed = vi.fn();
const tested = vi.fn();
const vendors: { current: RuntimeVendorConfigView[] } = { current: [] };
const readFailed: { current: unknown } = { current: null };

beforeEach(() => {
  readFailed.current = null;
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

vi.mock("../../hooks/useRuntimeVendors", () => ({
  useRuntimeVendors: () => ({
    data: readFailed.current ? undefined : vendors.current,
    isLoading: false,
    isError: !!readFailed.current,
    error: readFailed.current,
  }),
  useSaveRuntimeVendor: () => ({ mutateAsync: saved, isPending: false }),
  useDeleteRuntimeVendor: () => ({ mutateAsync: removed, isPending: false }),
  useTestRuntimeVendor: () => ({ mutateAsync: tested, isPending: false }),
}));

describe("CloudVendors", () => {
  it("lists configured vendors without exposing their token", () => {
    vendors.current = [vendor()];
    const { container } = render(<CloudVendors />);
    expect(screen.getByTestId("cloud-vendor-row-fly")).toBeTruthy();
    expect(screen.getByText("Token set")).toBeTruthy();
    // The row reports that a token exists and offers no way to see it; the
    // token input only appears once an edit is opened.
    expect(container.querySelector("input[type=password]")).toBeNull();
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
    render(<CloudVendors />);
    expect(screen.getByTestId("cloud-vendors-error").textContent).toContain(
      "Couldn’t load cloud vendors",
    );
    expect(screen.queryByText("No cloud vendors are configured.")).toBeNull();
  });

  it("still says so when there genuinely are none", () => {
    vendors.current = [];
    render(<CloudVendors />);
    expect(screen.queryByTestId("cloud-vendors-error")).toBeNull();
    expect(screen.queryByText("No cloud vendors are configured.")).not.toBeNull();
  });

  it("saves a new fly vendor as an adjacently tagged union", async () => {
    // The wire shape is `{kind, value}`, not a flat object with a kind field:
    // a flat body deserialises to nothing and fails as a 422.
    vendors.current = [];
    saved.mockClear();
    render(<CloudVendors />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add-fly"));

    const field = (label: string) =>
      screen.getByText(label).parentElement!.querySelector("input")!;
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
    vendors.current = [];
    saved.mockClear();
    render(<CloudVendors />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add-velos"));

    const field = (label: string) =>
      screen.getByText(label).parentElement!.querySelector("input")!;
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

  it("reports a vendor that answers", async () => {
    vendors.current = [vendor()];
    tested.mockClear().mockResolvedValue({ ok: true });
    render(<CloudVendors />);
    fireEvent.click(screen.getByTestId("cloud-vendor-test-fly"));

    await waitFor(() => expect(screen.getByTestId("cloud-vendor-ok-fly")).toBeTruthy());
    expect(tested).toHaveBeenCalledWith("fly");
  });

  it("shows what the substrate said when a check fails", async () => {
    // The failure this whole path exists for: a token that was good when it was
    // saved and is not any more. The row has to say so in the substrate's own
    // words — "unreachable" alone sends someone to check their network.
    vendors.current = [vendor()];
    tested
      .mockClear()
      .mockResolvedValue({ ok: false, error: "fly refused: 401: unauthorized" });
    render(<CloudVendors />);
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
    saved.mockClear();
    render(<CloudVendors />);
    fireEvent.click(screen.getByTestId("cloud-vendor-edit-fly"));
    fireEvent.change(
      screen.getByText("Region").parentElement!.querySelector("input")!,
      { target: { value: "lhr" } },
    );
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
