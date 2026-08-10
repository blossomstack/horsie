import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { PopoverMenu } from "./PopoverMenu";

afterEach(cleanup);

/** The focus hand-back is deferred by one task so it can see where the browser
 * settled; tests have to let that task run. */
async function settle() {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });
}

/** Three button-style choices, the shape the model/environment/workflow
 * pickers render. */
function Options({ close }: { close: () => void }) {
  return (
    <>
      {["haiku", "sonnet", "opus"].map((m) => (
        <button
          key={m}
          type="button"
          data-popover-option
          data-testid={`option-${m}`}
          aria-pressed={m === "sonnet"}
          onClick={close}
        >
          {m}
        </button>
      ))}
    </>
  );
}

function renderPicker() {
  return render(
    <PopoverMenu variant="icon" legend="Model" label="sonnet" testId="config-model">
      {(close) => <Options close={close} />}
    </PopoverMenu>,
  );
}

describe("PopoverMenu icon variant", () => {
  it("shows the setting name above the popup content", () => {
    const { getByTestId, getByText } = render(
      <PopoverMenu
        variant="icon"
        legend="Model"
        label="sonnet"
        testId="config-model"
      >
        {() => <div data-testid="model-options">sonnet</div>}
      </PopoverMenu>,
    );

    fireEvent.click(getByTestId("config-model"));

    const heading = getByText("Model");
    const options = getByTestId("model-options");
    expect(heading).toBeTruthy();
    expect(
      heading.compareDocumentPosition(options) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });
});

describe("PopoverMenu wiring", () => {
  it("names the panel and points the trigger at it", () => {
    const { getByTestId } = renderPicker();
    const trigger = getByTestId("config-model");
    expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");
    expect(trigger.getAttribute("aria-controls")).toBeNull();

    fireEvent.click(trigger);

    const panel = screen.getByRole("dialog");
    expect(panel.getAttribute("aria-label")).toBe("Model — sonnet");
    expect(trigger.getAttribute("aria-controls")).toBe(panel.id);
    expect(panel.id).toBeTruthy();
  });
});

describe("PopoverMenu focus", () => {
  it("hands focus back to the trigger when Escape closes it", async () => {
    const { getByTestId } = renderPicker();
    const trigger = getByTestId("config-model");
    fireEvent.click(trigger);
    getByTestId("option-opus").focus();

    fireEvent.keyDown(document, { key: "Escape" });
    await settle();

    expect(document.activeElement).toBe(trigger);
  });

  it("hands focus back to the trigger when an option is chosen", async () => {
    const { getByTestId } = renderPicker();
    const trigger = getByTestId("config-model");
    fireEvent.click(trigger);
    const option = getByTestId("option-opus");
    option.focus();

    fireEvent.click(option);
    await settle();

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("hands focus back to the trigger when a pointerdown outside closes it", async () => {
    const { getByTestId } = render(
      <div>
        <span data-testid="outside">outside</span>
        <PopoverMenu variant="icon" legend="Model" label="sonnet" testId="config-model">
          {(close) => <Options close={close} />}
        </PopoverMenu>
      </div>,
    );
    const trigger = getByTestId("config-model");
    fireEvent.click(trigger);
    getByTestId("option-opus").focus();

    fireEvent.pointerDown(getByTestId("outside"));
    await settle();

    expect(document.activeElement).toBe(trigger);
  });

  it("leaves focus alone when something else claimed it", async () => {
    const { getByTestId } = render(
      <div>
        <button type="button" data-testid="elsewhere">
          elsewhere
        </button>
        <PopoverMenu variant="icon" legend="Model" label="sonnet" testId="config-model">
          {(close) => <Options close={close} />}
        </PopoverMenu>
      </div>,
    );
    fireEvent.click(getByTestId("config-model"));
    const elsewhere = getByTestId("elsewhere");
    elsewhere.focus();

    fireEvent.pointerDown(elsewhere);
    await settle();

    expect(document.activeElement).toBe(elsewhere);
  });

  it("does not steal focus before it has ever been opened", async () => {
    const { getByTestId } = render(
      <div>
        <button type="button" data-testid="elsewhere">
          elsewhere
        </button>
        <PopoverMenu variant="icon" legend="Model" label="sonnet" testId="config-model">
          {(close) => <Options close={close} />}
        </PopoverMenu>
      </div>,
    );
    getByTestId("elsewhere").focus();
    await settle();
    expect(document.activeElement).toBe(getByTestId("elsewhere"));
  });
});

describe("PopoverMenu option roving", () => {
  it("gives the chosen option the only tab stop", () => {
    const { getByTestId } = renderPicker();
    fireEvent.click(getByTestId("config-model"));

    expect(getByTestId("option-haiku").getAttribute("tabindex")).toBe("-1");
    expect(getByTestId("option-sonnet").getAttribute("tabindex")).toBe("0");
    expect(getByTestId("option-opus").getAttribute("tabindex")).toBe("-1");
  });

  it("walks the options with the arrow keys and wraps at both ends", () => {
    const { getByTestId } = renderPicker();
    fireEvent.click(getByTestId("config-model"));
    const panel = screen.getByRole("dialog");
    getByTestId("option-sonnet").focus();

    fireEvent.keyDown(panel, { key: "ArrowDown" });
    expect(document.activeElement).toBe(getByTestId("option-opus"));
    expect(getByTestId("option-opus").getAttribute("tabindex")).toBe("0");
    expect(getByTestId("option-sonnet").getAttribute("tabindex")).toBe("-1");

    fireEvent.keyDown(panel, { key: "ArrowDown" });
    expect(document.activeElement).toBe(getByTestId("option-haiku"));

    fireEvent.keyDown(panel, { key: "ArrowUp" });
    expect(document.activeElement).toBe(getByTestId("option-opus"));

    fireEvent.keyDown(panel, { key: "End" });
    expect(document.activeElement).toBe(getByTestId("option-opus"));
    fireEvent.keyDown(panel, { key: "Home" });
    expect(document.activeElement).toBe(getByTestId("option-haiku"));
  });

  it("opens on ArrowDown from the trigger and takes the focus with it", () => {
    const { getByTestId } = renderPicker();
    const trigger = getByTestId("config-model");
    trigger.focus();

    fireEvent.keyDown(trigger, { key: "ArrowDown" });

    expect(screen.getByRole("dialog")).toBeTruthy();
    expect(document.activeElement).toBe(getByTestId("option-haiku"));
  });

  it("keeps its hands off native controls, which already have arrow keys", () => {
    const { getByTestId } = render(
      <PopoverMenu variant="icon" legend="Thinking" label="high" testId="config-thinking">
        {() => (
          <>
            <input type="radio" name="effort" data-testid="effort-low" />
            <input type="radio" name="effort" data-testid="effort-high" />
          </>
        )}
      </PopoverMenu>,
    );
    fireEvent.click(getByTestId("config-thinking"));
    const low = getByTestId("effort-low");
    low.focus();

    fireEvent.keyDown(screen.getByRole("dialog"), { key: "ArrowDown" });

    // No roving tabindex written over them, and the browser's own radio
    // navigation is left to do the moving.
    expect(low.getAttribute("tabindex")).toBeNull();
    expect(document.activeElement).toBe(low);
  });
});
