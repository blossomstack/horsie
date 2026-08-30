import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ArtifactRef,
  CreateSessionRequest,
  CreateSessionResponse,
  SettingsView,
} from "../api/types";
import { DRAFT_STORAGE_KEY, type DraftPayload } from "../hooks/draftPersistence";
import { settingsKey } from "../hooks/useSettings";
import { workflowKeys } from "../hooks/useWorkflows";
import { NewSessionView } from "./NewSessionView";

// The two calls this page makes that leave the browser: the upload the
// composer starts on attach, and the create the send performs. Mocked at the
// module so the assertion is about *what was sent*, not about the request the
// client builds around it.
const upload = vi.fn<(file: File) => Promise<ArtifactRef>>();
const create = vi.fn<(body: CreateSessionRequest) => Promise<CreateSessionResponse>>();
vi.mock("../api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../api/client")>();
  return {
    ...actual,
    api: {
      ...actual.api,
      artifacts: { upload: (file: File) => upload(file) },
      // The real one throws unless a project is selected, which is routing
      // this page has no part in.
      artifactUrl: (id: string) => `/api/p/test/artifacts/${id}`,
      sessions: {
        ...actual.api.sessions,
        create: (body: CreateSessionRequest) => create(body),
      },
    },
  };
});

const imageRef = (id = "sha-1"): ArtifactRef => ({
  id,
  mediaType: "image/png",
  byteSize: 1234,
  kind: { kind: "Image", value: { width: 800, height: 600 } },
  filename: "shot.png",
});

const png = () => new File(["bytes"], "shot.png", { type: "image/png" });

/** Enough of `/api/config` for the draft to be sendable: a model and a
 * runtime, which are the two things `blockedReason` insists on. */
const settings: SettingsView = {
  providers: [],
  models: [{ alias: "sonnet", provider: "p", modelId: "m1" }],
  vendors: [
    { name: "local", isDefault: true, capabilities: { supportsProvisioning: false } },
  ],
  defaultRuntimeVendor: "local",
  info: {
    configPath: "",
    database: "",
    stateDir: "",
    dataDir: "",
    pluginsDir: "",
    version: "0",
  },
  restartRequired: false,
};

function storeDraft(draft: Partial<DraftPayload> = {}) {
  const full: DraftPayload = {
    v: 2,
    environment: { kind: "runtime", vendor: "local", repos: {} },
    model: "sonnet",
    skills: [],
    mcp: [],
    memorySpaces: [],
    tools: null,
    thinkingEffort: "",
    artifacts: [],
    ...draft,
  };
  localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(full));
}

function renderPage(route = "/") {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  client.setQueryData(settingsKey, settings);
  client.setQueryData(workflowKeys.all, [
    { name: "triage", description: "", start: "a", steps: [], createdAt: "0", updatedAt: "0" },
  ]);
  render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[route]}>
        <NewSessionView />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return screen.getByTestId("composer-input");
}

/** Paste with files and nothing else — a screenshot on the clipboard. */
const pasteFiles = (input: HTMLElement, files: File[]) =>
  fireEvent.paste(input, { clipboardData: { files, items: [], types: [] } });

beforeEach(() => {
  localStorage.clear();
  upload.mockReset();
  upload.mockResolvedValue(imageRef());
  create.mockReset();
  create.mockResolvedValue({
    session: {
      id: "s1",
      name: undefined,
      status: "provisioning",
      createdAt: 0,
      annotations: [],
      vendor: "local",
      subSessions: [],
    },
  } as unknown as CreateSessionResponse);
  URL.createObjectURL = vi.fn(() => "blob:preview");
  URL.revokeObjectURL = vi.fn();
});

afterEach(cleanup);

describe("NewSessionView attachments", () => {
  // The case this page existed to refuse: a session is created *with* its
  // first message, so a screenshot pasted here had nowhere to go and the
  // composer used to turn attaching off rather than drop it. Both halves are
  // asserted — that the file is accepted at all, and that the create carries
  // it — because the first one passing alone is exactly the old bug.
  it("starts a session with a pasted screenshot on its first message", async () => {
    storeDraft();
    const input = renderPage();

    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(screen.getByTestId("composer-attachment").dataset.status).toBe("ready"),
    );
    fireEvent.change(input, { target: { value: "what is in this?" } });
    fireEvent.click(screen.getByTestId("composer-send"));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    const body = create.mock.calls[0][0];
    expect(body.message).toBe("what is in this?");
    expect(body.artifacts).toEqual([imageRef()]);
  });

  // A workflow run is handed an input, not a message, and its request has
  // nowhere to put a file. Refused out loud: dropping it silently is the very
  // bug the create path was changed to fix, and the composer keeps the
  // attachment because the send was rejected.
  it("refuses a run that carries an attachment rather than dropping it", async () => {
    storeDraft();
    const input = renderPage("/?workflow=triage");
    await waitFor(() => expect(screen.getByTestId("workflow-run-banner")).toBeTruthy());

    pasteFiles(input, [png()]);
    await waitFor(() =>
      expect(screen.getByTestId("composer-attachment").dataset.status).toBe("ready"),
    );
    fireEvent.change(input, { target: { value: "ship it" } });
    fireEvent.click(screen.getByTestId("composer-send"));

    // The *attachment* refusal, named: without this the same banner appears
    // for any failed run and the assertion would pass on the old behaviour.
    await waitFor(() =>
      expect(screen.getByTestId("session-error").textContent).toContain(
        "Attachments",
      ),
    );
    expect(create).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.getByTestId("composer-attachment")).toBeTruthy());
  });

  // Nothing attached is still a create, and it must send the field rather
  // than omit it — one shape for the server to read.
  it("sends an empty artifact list when nothing is attached", async () => {
    storeDraft();
    const input = renderPage();

    fireEvent.change(input, { target: { value: "just talking" } });
    fireEvent.click(screen.getByTestId("composer-send"));

    await waitFor(() => expect(create).toHaveBeenCalledTimes(1));
    expect(create.mock.calls[0][0].artifacts).toEqual([]);
  });
});
