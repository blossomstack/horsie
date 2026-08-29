import { createRoot } from "react-dom/client";
import "./index.css";
import "./i18n";
import { setCurrentProject } from "./api/client";
import { Composer } from "./components/Composer";
import { Transcript } from "./components/Transcript";
import { ToolCallCard } from "./components/ToolCallCard";
import { SessionStatusKind, type ArtifactRef } from "./api/types";
import type { TranscriptItem } from "./hooks/useSessionStream";

setCurrentProject("demo");

const img = (w: number, h: number, id: string): ArtifactRef => ({
  id, mediaType: "image/png", byteSize: 812_000,
  kind: { kind: "Image", value: { width: w, height: h } }, filename: "screenshot.png",
});
const doc: ArtifactRef = {
  id: "d1", mediaType: "application/pdf", byteSize: 2_400_000,
  kind: { kind: "Document", value: {} }, filename: "protocol-spec.pdf",
};

const items: TranscriptItem[] = [
  { kind: "message", value: { id: "u1", role: "User", text: "What is wrong with this layout?", thinking: [], toolCalls: [], subagentResults: [], artifacts: [img(640, 400, "a"), doc], createdAtMs: Date.now() } },
  { kind: "message", value: { id: "a1", role: "Assistant", text: "The tray runs under the button lane. Here is the render I took:", thinking: [], toolCalls: [], subagentResults: [], artifacts: [img(300, 420, "b")], createdAtMs: Date.now() } },
];

const call = {
  id: "t1", name: "screenshot", input: { url: "https://horsie.dev" },
  output: "", isError: false, running: false, hooks: [], artifacts: [img(500, 300, "c")],
};

createRoot(document.getElementById("root")!).render(
  <div className="min-h-screen bg-chassis">
    <Transcript items={items} streaming="" orphanTools={[]} showLive={false} showThinking={false} sessionId="s1" />
    <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
      <div className="panel p-3"><ToolCallCard call={call} /></div>
    </div>
    <div className="mt-6" id="composer-slot">
      <Composer status={SessionStatusKind.Idle} busy={false} entries={[]} onSend={() => {}} onStop={() => {}} />
    </div>
  </div>,
);
