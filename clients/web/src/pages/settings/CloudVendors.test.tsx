import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RuntimeVendorConfigView } from "../../api/types";
import { CloudVendors } from "./CloudVendors";

afterEach(cleanup);

const saved = vi.fn();
const removed = vi.fn();
const vendors: { current: RuntimeVendorConfigView[] } = { current: [] };

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
  useRuntimeVendors: () => ({ data: vendors.current, isLoading: false }),
  useSaveRuntimeVendor: () => ({ mutateAsync: saved, isPending: false }),
  useDeleteRuntimeVendor: () => ({ mutateAsync: removed, isPending: false }),
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

  it("saves a new vendor as an adjacently tagged union", async () => {
    // The wire shape is `{kind, value}`, not a flat object with a kind field:
    // a flat body deserialises to nothing and fails as a 422.
    vendors.current = [];
    saved.mockClear();
    render(<CloudVendors />);
    fireEvent.click(screen.getByTestId("cloud-vendor-add"));

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
            callbackUrl: "wss://horsie.example.com",
          }),
        },
        credential: "tok",
      },
    });
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
