import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// Testing Library unmounts after each test only when the runner has global hooks; vitest's are off.
afterEach(() => {
  cleanup();
});

// jsdom does not implement scrolling; the router asks for it after each navigation.
window.scrollTo = () => {};
