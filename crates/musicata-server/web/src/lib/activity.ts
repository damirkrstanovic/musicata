// SPDX-License-Identifier: AGPL-3.0-or-later
import type { Activity } from "../types/Activity";

/** Subscribe to the server's activity feed over WebSocket, reconnecting with a 3s backoff.
 *  Returns a disposer that stops reconnecting and closes the socket. */
export function connectActivity(onMessage: (items: Activity[]) => void): () => void {
  let socket: WebSocket | null = null;
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const connect = () => {
    if (closed) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    try {
      socket = new WebSocket(`${scheme}://${location.host}/api/activity/ws`);
    } catch {
      timer = setTimeout(connect, 3000);
      return;
    }
    socket.onmessage = (event) => {
      try {
        onMessage(JSON.parse(event.data) as Activity[]);
      } catch {
        // ignore malformed frames
      }
    };
    socket.onclose = () => {
      if (!closed) timer = setTimeout(connect, 3000);
    };
    socket.onerror = () => {
      try {
        socket?.close();
      } catch {
        // already closing
      }
    };
  };

  connect();
  return () => {
    closed = true;
    if (timer) clearTimeout(timer);
    try {
      socket?.close();
    } catch {
      // ignore
    }
  };
}
