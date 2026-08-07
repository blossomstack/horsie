import { cleanup, fireEvent, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { PopoverMenu } from "./PopoverMenu";

afterEach(cleanup);

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
