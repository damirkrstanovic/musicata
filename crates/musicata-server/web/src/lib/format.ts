export function timeAgo(unix: number | null | undefined): string {
  if (!unix) return "";
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - unix);
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ago`;
  return `${Math.floor(secs / 3600)}h ago`;
}

export function pct(part: number, total: number): number {
  return total ? Math.round((part / total) * 100) : 0;
}
