// SPDX-License-Identifier: AGPL-3.0-or-later
import type { SnapcastHealth } from "../types/SnapcastHealth";

/** Latest observations only; never infer physical audio health from transport state. */
export function connectSnapcastHealth(receive: (value: SnapcastHealth) => void, disconnected: () => void): () => void {
  let socket: WebSocket | null = null;
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let staleTimer: ReturnType<typeof setTimeout> | undefined;
  const stale = () => { clearTimeout(staleTimer); staleTimer = setTimeout(disconnected, 15000); };
  const connect = () => {
    if (closed) return;
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const current = new WebSocket(`${scheme}://${location.host}/api/snapcast/health/ws`);
    socket = current;
    stale();
    current.onmessage = event => {
      if (closed || socket !== current) return;
      try {
        const value = JSON.parse(event.data);
        if (!value || typeof value.observed_at !== "number" || !Array.isArray(value.clients) || !Array.isArray(value.streams)) return;
        receive(value as SnapcastHealth);
        stale();
      } catch { /* Await the next valid observation. */ }
    };
    current.onclose = () => {
      if (closed || socket !== current) return;
      clearTimeout(staleTimer);
      disconnected();
      timer = setTimeout(connect, 3000);
    };
    current.onerror = () => current.close();
  };
  connect();
  return () => {
    closed = true;
    clearTimeout(timer);
    clearTimeout(staleTimer);
    socket?.close();
  };
}
