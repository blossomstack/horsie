import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Menu, MenuItem } from "./Menu";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

describe("Menu", () => {
  it("opens on trigger, selects an item, and closes", () => {
    const onSelect = vi.fn();
    render(
      <Menu label="group actions">
        <MenuItem onSelect={onSelect}>Rename</MenuItem>
      </Menu>,
    );
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "group actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));
    expect(onSelect).toHaveBeenCalledOnce();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on Escape and on outside pointerdown", () => {
    render(
      <div>
        <span data-testid="outside">outside</span>
        <Menu label="group actions">
          <MenuItem onSelect={() => {}}>Rename</MenuItem>
        </Menu>
      </div>,
    );
    const trigger = screen.getByRole("button", { name: "group actions" });
    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(trigger);
    fireEvent.pointerDown(screen.getByTestId("outside"));
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
