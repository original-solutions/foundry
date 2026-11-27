import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// Testing Library unmounts after each test only when the runner has global hooks; vitest's are off.
afterEach(() => {
  cleanup();
});

// jsdom does not implement scrolling; the router asks for it after each navigation. The guide test
// runs under node, with no window.
if (typeof window !== "undefined") window.scrollTo = () => {};
