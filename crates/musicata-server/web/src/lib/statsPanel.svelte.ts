// Tiny UI state for the Listening-stats overlay (open/closed). The figures themselves are
// fetched fresh from GET /api/history/stats when the panel opens (see StatsPanel.svelte).
class StatsPanel {
  open = $state(false);

  toggle(): void {
    this.open = !this.open;
  }
}

export const statsPanel = new StatsPanel();
