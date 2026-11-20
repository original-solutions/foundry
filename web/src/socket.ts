/** The WebSocket URL of daemon API `path` (such as `/api/events`) on the host that served the page. */
export function socketUrl(path: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}${path}`;
}

/** The wait before trying a socket again: 500 ms, doubling each time up to 5 s, until reset. */
export class Backoff {
  private wait = 500;

  next(): number {
    const wait = this.wait;
    this.wait = Math.min(wait * 2, 5000);
    return wait;
  }

  reset() {
    this.wait = 500;
  }
}
