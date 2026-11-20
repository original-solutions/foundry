/**
 * This browser's storage, for the choices and memories the page keeps (a theme, a fold, what was
 * viewed). Storage can be blocked or full; then nothing is read or kept and the page goes on, so
 * no caller handles that itself.
 */

/** The text kept under `key`, or null when there is none or storage is blocked. */
export function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** What is kept under `key` read as JSON, or undefined when there is none, it is not JSON or storage is blocked. */
export function readStoredJson(key: string): unknown {
  const text = readStored(key);
  if (text === null) return undefined;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** Keeps `value` under `key`, and says whether it was kept: blocked or full storage keeps nothing. */
export function store(key: string, value: string): boolean {
  try {
    localStorage.setItem(key, value);
    return true;
  } catch {
    return false;
  }
}

/** `store` of `value` as JSON. */
export function storeJson(key: string, value: unknown): boolean {
  return store(key, JSON.stringify(value));
}

/** Takes `key` out, if storage lets it. */
export function unstore(key: string) {
  try {
    localStorage.removeItem(key);
  } catch {
    // Blocked: nothing was kept to take out.
  }
}
