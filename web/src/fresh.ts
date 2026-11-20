/**
 * The UI is built into the daemon, so a page left open across an upgrade runs the old bundle
 * against the new daemon, whose answers it may not read. Each time the events socket opens again
 * (as it does after the daemon restarts), the page reads the daemon's index page and reloads
 * when it now loads another bundle.
 */

/** The path of the module script `doc` loads, or none. */
function bundleOf(doc: Document): string | null {
  const src = doc.querySelector<HTMLScriptElement>('script[type="module"][src]')?.getAttribute("src");
  return src ? new URL(src, window.location.href).pathname : null;
}

/** The bundle the daemon's index page loads now, or none when it cannot be read. */
async function servedBundle(): Promise<string | null> {
  try {
    const response = await fetch("/", { cache: "no-store" });
    if (!response.ok) return null;
    return bundleOf(new DOMParser().parseFromString(await response.text(), "text/html"));
  } catch {
    return null;
  }
}

/** Calls `reload` when the daemon now serves another bundle than the one this page runs. */
export async function reloadIfStale(reload: () => void = () => window.location.reload()): Promise<void> {
  const loaded = bundleOf(document);
  if (!loaded) return;
  const served = await servedBundle();
  if (served && served !== loaded) reload();
}
