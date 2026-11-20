import { socketUrl } from "./socket";

/**
 * Types `text` into terminal `id` as one paste and presses Enter, over a socket of its own that
 * closes once both are sent. It never resizes the terminal, so a pane showing it is left alone.
 * Rejects when the terminal cannot be reached.
 */
export function reply(id: string, text: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(socketUrl(`/api/terminals/${id}`));
    socket.binaryType = "arraybuffer";
    const bytes = new TextEncoder();
    socket.onopen = () => {
      socket.send(bytes.encode(`\x1b[200~${text}\x1b[201~`));
      socket.send(bytes.encode("\r"));
      socket.close(1000);
      resolve();
    };
    socket.onclose = (event) => reject(new Error(event.reason || "the terminal closed"));
  });
}
