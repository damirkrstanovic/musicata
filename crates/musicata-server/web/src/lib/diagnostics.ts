// SPDX-License-Identifier: AGPL-3.0-or-later
// Fixed categories, bounded history and latest undelivered state; no private exception data.
type Category = "browser.audio" | "browser.buffering" | "browser.graph" | "browser.routing" | "browser.dsp";
type Report = { category: Category; failed: boolean; message: string; at: number; token: string | null; dirty: boolean };
const last = new Map<Category, Report>();
const pending: Report[] = [];
let running = false;
let lost = 0;
let token: string | null = null;
let previousToken: string | null = null;
export function setDiagnosticRendererToken(next: string | null): void {
  token = next;
  if (next && previousToken && next !== previousToken) {
    for (let index = pending.length - 1; index >= 0; index--) {
      if (pending[index].token && pending[index].token !== next) { pending.splice(index, 1); lost++; }
    }
    for (const [category, report] of last) if (report.token && report.token !== next) { last.delete(category); if (report.dirty) lost++; }
  }
  if (next) { previousToken = next; void drain(); }
}
async function drain(): Promise<void> {
  if (running || !token) return;
  running = true;
  try {
    while (token) {
      const report = pending.shift() ?? [...last.values()].find(report => report.dirty);
      if (!report) break;
      if (report.token && report.token !== token) { lost++; report.dirty = false; continue; }
      report.token = token;
      const lostAtStart = Math.min(lost, 1000000);
      let delivered = false;
      let delay = 2000;
      for (let attempt = 0; attempt < 3; attempt++) {
        try {
          const response = await fetch("/api/diagnostics/reports", {
            method: "POST", headers: { "content-type": "application/json", "x-musicata-renderer": report.token },
            body: JSON.stringify({ category: report.category, component: "browser", action: report.failed ? "failure" : "recovery", context: {}, message: report.message, measurements: lostAtStart ? { lost_reports: lostAtStart } : {} }),
            signal: AbortSignal.timeout(3000),
          });
          if (response.ok) { delivered = true; break; }
          if (response.status === 429) { delay = 60000; break; }
          if (response.status === 401 || response.status === 403) { token = null; break; }
        } catch { /* Reporting cannot interfere with audio. */ }
        if (attempt < 2) await new Promise(resolve => setTimeout(resolve, 2000));
      }
      if (delivered) {
        lost = Math.max(0, lost - lostAtStart);
        report.dirty = false;
      } else {
        if (last.get(report.category) !== report) lost++;
        // Latest state remains eligible even if its original enqueue/delivery failed.
        if (token) await new Promise(resolve => setTimeout(resolve, delay));
      }
    }
  } finally { running = false; }
}
export function reportDiagnostic(category: Category, failed: boolean, message = "operation failed"): void {
  const previous = last.get(category);
  const at = performance.now();
  if (!failed && !previous?.failed) return;
  if (previous?.failed === failed && at - previous.at < 60000) return;
  const report: Report = { category, failed, message, at, token, dirty: true };
  last.set(category, report);
  if (pending.length >= 32) { lost++; return; }
  pending.push(report);
  void drain();
}
