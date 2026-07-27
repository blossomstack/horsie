import { Brain, FolderPlus, Loader2, Plus, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { ApiRequestError } from "../api/client";
import type { MemorySpaceView, MemoryView } from "../api/types";
import { cn } from "../lib/cn";
import {
  useCreateMemory,
  useCreateSpace,
  useDeleteMemory,
  useDeleteSpace,
  useMemories,
  useMemorySpaces,
  useUpdateMemory,
} from "../hooks/useMemory";

/**
 * Manage the agent's long-term memories: spaces on the left, the selected
 * space's memories on the right. The agent writes here through its `memory_*`
 * tools; this page is where a human curates what it wrote.
 */
export function MemoryPage() {
  const spaces = useMemorySpaces();
  const [picked, setPicked] = useState<string | null>(null);
  // Fall back to the first space until the user picks one, so the page is
  // useful on first load without a click.
  const active =
    picked && spaces.data?.some((s) => s.name === picked)
      ? picked
      : (spaces.data?.[0]?.name ?? null);
  const memories = useMemories(active ?? undefined);

  const createSpace = useCreateSpace();
  const [newSpace, setNewSpace] = useState("");

  const submitSpace = async () => {
    const name = newSpace.trim();
    if (!name) return;
    try {
      await createSpace.mutateAsync({ name });
      setNewSpace("");
      setPicked(name);
    } catch {
      /* surfaced from createSpace.error below */
    }
  };

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center gap-3 border-b px-6 py-3.5">
        <div>
          <h1 className="text-[15px] font-semibold text-text">Memory</h1>
          <p className="text-xs text-faint">
            Durable notes the agent saves and reads back — grouped into spaces
            you pick per session.
          </p>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          <section className="card p-4">
            <SectionHeading
              icon={<FolderPlus size={15} className="mt-0.5 text-faint" />}
              title="Memory spaces"
              subtitle="A space is a namespace of memories. Sessions choose which ones they can read and write."
            />

            <div className="grid grid-cols-[1fr_auto] gap-3">
              <label className="block">
                <span className="mb-1 block text-[11px] font-semibold text-muted">
                  New space
                </span>
                <input
                  className="input font-mono"
                  value={newSpace}
                  onChange={(e) => setNewSpace(e.target.value)}
                  placeholder="ops"
                />
              </label>
              <div className="flex items-end">
                <button
                  className="btn-primary"
                  onClick={submitSpace}
                  disabled={!newSpace.trim() || createSpace.isPending}
                >
                  {createSpace.isPending ? (
                    <Loader2 size={15} className="animate-spin" />
                  ) : (
                    <Plus size={15} />
                  )}
                  Create
                </button>
              </div>
            </div>

            <ErrorNote
              error={createSpace.error}
              fallback="Failed to create space."
            />

            <div className="mt-3 space-y-2.5">
              {spaces.isLoading && (
                <p className="py-8 text-center text-sm text-faint">Loading…</p>
              )}
              {spaces.isError && (
                <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
                  Couldn’t load memory spaces. Is the server running?
                </div>
              )}
              {spaces.data?.length === 0 && (
                <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
                  No memory spaces yet. Create one above.
                </p>
              )}
              {spaces.data?.map((s) => (
                <SpaceRow
                  key={s.name}
                  space={s}
                  active={s.name === active}
                  onSelect={() => setPicked(s.name)}
                />
              ))}
            </div>
          </section>

          <section className="card p-4">
            <SectionHeading
              icon={<Brain size={15} className="mt-0.5 text-faint" />}
              title={active ? `Memories in ${active}` : "Memories"}
              subtitle="The agent writes these itself. Edit or delete anything that is wrong or no longer useful."
            />

            {!active ? (
              <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
                Create a memory space first.
              </p>
            ) : (
              <>
                <NewMemoryForm space={active} />

                <div className="mt-3 space-y-2.5">
                  {memories.isLoading && (
                    <p className="py-8 text-center text-sm text-faint">
                      Loading…
                    </p>
                  )}
                  {memories.data?.length === 0 && (
                    <p className="rounded-[var(--radius)] border border-dashed px-3 py-4 text-center text-sm text-faint">
                      No memories in this space yet.
                    </p>
                  )}
                  {memories.data?.map((m) => (
                    <MemoryRow key={m.id} memory={m} />
                  ))}
                </div>
              </>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function SpaceRow({
  space,
  active,
  onSelect,
}: {
  space: MemorySpaceView;
  active: boolean;
  onSelect: () => void;
}) {
  const remove = useDeleteSpace();

  const confirmDelete = () => {
    const tail =
      space.memoryCount === 0
        ? "It holds no memories."
        : `This also deletes its ${space.memoryCount} ${
            space.memoryCount === 1 ? "memory" : "memories"
          }.`;
    if (confirm(`Delete memory space "${space.name}"? ${tail}`))
      remove.mutate(space.name);
  };

  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-[var(--radius)] border p-3",
        active && "border-accent",
      )}
      style={{ background: "var(--surface-2)" }}
    >
      <button
        type="button"
        className="min-w-0 flex-1 text-left"
        onClick={onSelect}
      >
        <span className="truncate font-mono text-sm font-semibold text-text">
          {space.name}
        </span>
        {space.description && (
          <p className="mt-0.5 text-xs text-muted">{space.description}</p>
        )}
        <p className="mt-0.5 text-[11px] text-faint">
          {space.memoryCount} {space.memoryCount === 1 ? "memory" : "memories"}
        </p>
      </button>
      <button
        className="btn-icon shrink-0 text-faint hover:text-error"
        onClick={confirmDelete}
        disabled={remove.isPending}
        aria-label="Delete space"
      >
        <Trash2 size={15} />
      </button>
    </div>
  );
}

function NewMemoryForm({ space }: { space: string }) {
  const create = useCreateMemory();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [content, setContent] = useState("");

  const submit = async () => {
    if (!name.trim() || !description.trim() || !content.trim()) return;
    try {
      await create.mutateAsync({
        space,
        name: name.trim(),
        description: description.trim(),
        content,
      });
      setName("");
      setDescription("");
      setContent("");
    } catch {
      /* surfaced from create.error below */
    }
  };

  return (
    <div className="mb-4 rounded-[var(--radius)] border border-dashed p-3">
      <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,2fr)] gap-3">
        <label className="block">
          <span className="mb-1 block text-[11px] font-semibold text-muted">
            Name
          </span>
          <input
            className="input font-mono"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="deploy-order"
          />
        </label>
        <label className="block">
          <span className="mb-1 block text-[11px] font-semibold text-muted">
            Description (one line, shown to the agent)
          </span>
          <input
            className="input"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="velos must be up before the server"
          />
        </label>
      </div>
      <label className="mt-3 block">
        <span className="mb-1 block text-[11px] font-semibold text-muted">
          Content
        </span>
        <textarea
          className="input min-h-24 font-mono text-xs"
          value={content}
          onChange={(e) => setContent(e.target.value)}
          placeholder="Markdown. Reference another memory as [[space/name]]."
        />
      </label>

      <ErrorNote error={create.error} fallback="Failed to save memory." />

      <div className="mt-3 flex justify-end">
        <button
          className="btn-primary"
          onClick={submit}
          disabled={
            !name.trim() ||
            !description.trim() ||
            !content.trim() ||
            create.isPending
          }
        >
          {create.isPending ? (
            <Loader2 size={15} className="animate-spin" />
          ) : (
            <Plus size={15} />
          )}
          Save memory
        </button>
      </div>
    </div>
  );
}

function MemoryRow({ memory }: { memory: MemoryView }) {
  const update = useUpdateMemory();
  const remove = useDeleteMemory();
  const [open, setOpen] = useState(false);
  const [description, setDescription] = useState(memory.description);
  const [content, setContent] = useState(memory.content);

  const dirty =
    description !== memory.description || content !== memory.content;

  const save = () => {
    // Send only what actually changed — `MemoryUpdateInput`'s fields are
    // optional and an omitted one is left untouched server-side.
    update.mutate({
      id: memory.id,
      body: {
        description:
          description === memory.description ? undefined : description,
        content: content === memory.content ? undefined : content,
      },
    });
  };

  return (
    <div
      className="rounded-[var(--radius)] border p-3"
      style={{ background: "var(--surface-2)" }}
    >
      <div className="flex items-start gap-3">
        <button
          type="button"
          className="min-w-0 flex-1 text-left"
          onClick={() => setOpen((v) => !v)}
        >
          <span className="truncate font-mono text-sm font-semibold text-text">
            {memory.space}/{memory.name}
          </span>
          <p className="mt-0.5 text-xs text-muted">{memory.description}</p>
        </button>
        <button
          className="btn-icon shrink-0 text-faint hover:text-error"
          onClick={() => {
            if (confirm(`Delete memory "${memory.space}/${memory.name}"?`))
              remove.mutate(memory.id);
          }}
          disabled={remove.isPending}
          aria-label="Delete memory"
        >
          <Trash2 size={15} />
        </button>
      </div>

      {open && (
        <div className="mt-3 border-t pt-3">
          <label className="block">
            <span className="mb-1 block text-[11px] font-semibold text-muted">
              Description
            </span>
            <input
              className="input"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
            />
          </label>
          <label className="mt-3 block">
            <span className="mb-1 block text-[11px] font-semibold text-muted">
              Content
            </span>
            <textarea
              className="input min-h-32 font-mono text-xs"
              value={content}
              onChange={(e) => setContent(e.target.value)}
            />
          </label>

          <ErrorNote error={update.error} fallback="Failed to update memory." />

          <div className="mt-3 flex justify-end">
            <button
              className="btn-primary"
              onClick={save}
              disabled={!dirty || update.isPending}
            >
              {update.isPending && (
                <Loader2 size={15} className="animate-spin" />
              )}
              Save changes
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

function SectionHeading({
  icon,
  title,
  subtitle,
}: {
  icon: ReactNode;
  title: string;
  subtitle: string;
}) {
  return (
    <div className="mb-3 flex items-start gap-2">
      {icon}
      <div>
        <h2 className="text-sm font-semibold text-text">{title}</h2>
        <p className="mt-0.5 text-xs text-faint">{subtitle}</p>
      </div>
    </div>
  );
}

function ErrorNote({
  error,
  fallback,
}: {
  error: unknown;
  fallback: string;
}) {
  if (!error) return null;
  const message = error instanceof ApiRequestError ? error.message : fallback;
  return (
    <div className="mt-3 rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
      {message}
    </div>
  );
}
