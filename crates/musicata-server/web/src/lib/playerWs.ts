// One WebSocket per player (here: the browser player). Routes the two inbound message kinds:
// the lightweight `type:"progress"` tick (hot path) vs. the full PlaybackState snapshot.
// Reconnects with a 2s backoff. The returned `send` carries the browser's own progress/ended
// reports back up to the server.
import type { PlaybackState } from "../types/PlaybackState";

export interface ProgressTick {
  type: "progress";
  elapsed_seconds: number;
  duration_seconds: number | null;
}

export interface PlayerSocket {
  send: (msg: unknown) => void;
  close: () => void;
}

export function connectPlayer(
  kind: "player" | "zone",
  id: string,
  handlers: {
    onState: (s: PlaybackState) => void;
    onProgress: (t: ProgressTick) => void;
    onDisconnect?: () => void;
  },
): PlayerSocket {
  let socket: WebSocket | null = null;
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const connect = () => {
    if (closed) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    socket = new WebSocket(`${scheme}://${location.host}/api/${kind}s/${encodeURIComponent(id)}/ws`);
    socket.onmessage = (event) => {
      let msg: { type?: string };
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      if (msg.type === "progress") handlers.onProgress(msg as unknown as ProgressTick);
      else handlers.onState(msg as unknown as PlaybackState);
    };
    socket.onclose = () => {
      // Unexpected close = the server went away. Tell the app so it can stop local output
      // (a deliberate teardown/target-switch sets `closed` first and skips this).
      if (!closed) {
        handlers.onDisconnect?.();
        timer = setTimeout(connect, 2000);
      }
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

  return {
    send: (msg) => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(msg));
    },
    close: () => {
      closed = true;
      if (timer) clearTimeout(timer);
      try {
        socket?.close();
      } catch {
        // ignore
      }
    },
  };
}
