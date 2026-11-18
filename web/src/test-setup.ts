import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// Testing Library unmounts after each test only when the runner has global hooks; vitest's are off.
afterEach(() => {
  cleanup();
});
