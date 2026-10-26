<script lang="ts">
  // SPDX-License-Identifier: AGPL-3.0-or-later
  import { api, ApiError, type Player } from "../lib/api";

  let { player, onchange }: { player: Player; onchange: () => Promise<void> } = $props();

  function splitAddress(address: string): { host: string; port: number } {
    const ipv6 = address.match(/^\[([^\]]+)\]:(\d+)$/);
    if (ipv6) return { host: ipv6[1], port: Number(ipv6[2]) };
    const separator = address.lastIndexOf(":");
    if (separator > 0) {
      const port = Number(address.slice(separator + 1));
      if (Number.isInteger(port)) return { host: address.slice(0, separator), port };
    }
    return { host: address, port: 6600 };
  }

  let open = $state(false);
  let host = $state("");
  let port = $state(6600);
  let password = $state("");
  let clearPassword = $state(false);
  let busy = $state(false);
  let message = $state("");
  let failed = $state(false);

  function begin() {
    const parsed = splitAddress(player.address);
    host = parsed.host;
    port = parsed.port;
    password = "";
    clearPassword = false;
    message = "";
    failed = false;
    open = true;
  }

  async function save() {
    if (!host.trim() || !Number.isInteger(port) || port < 1 || port > 65535) {
      message = "Enter a hostname or IP address and a port from 1 to 65535.";
      failed = true;
      return;
    }
    busy = true;
    message = "Saving connection…";
    failed = false;
    try {
      const body: { host: string; port: number; password?: string } = { host: host.trim(), port };
      if (clearPassword) body.password = "";
      else if (password) body.password = password;
      const updated = await api.updatePlayerConnection(player.id, body);
      if (!updated) throw new Error("Missing player state");
      message = `${updated.address} saved. Musicata reconnects in the background; the queue stays in place.`;
      password = "";
      clearPassword = false;
      await onchange();
    } catch (error) {
      message = error instanceof ApiError ? error.message : "The connection could not be saved. Try again.";
      failed = true;
    } finally {
      busy = false;
    }
  }
</script>

{#if !open}
  <button type="button" class="ghost-button" onclick={begin}>Connection</button>
{:else}
  <form class="connection-editor" onsubmit={(event) => { event.preventDefault(); void save(); }}>
    <div class="connection-editor-head"><strong>MPD connection</strong><button type="button" class="ghost-button" disabled={busy} onclick={() => (open = false)}>Close</button></div>
    <p class="admin-hint">Saving reconnects this player. It keeps its queue, zone, sound settings, and access token.</p>
    <div class="field-grid">
      <label class="field field-host"><span>Hostname or IP address</span><input bind:value={host} placeholder="kitchen.local or 192.168.1.36" disabled={busy} /></label>
      <label class="field field-port"><span>Port</span><input type="number" min="1" max="65535" bind:value={port} disabled={busy} /></label>
      <label class="field field-password"><span>New password <em>(optional)</em></span><input type="password" bind:value={password} placeholder="Leave blank to keep the saved password" disabled={busy || clearPassword} /></label>
    </div>
    <label class="clear-password"><input type="checkbox" bind:checked={clearPassword} disabled={busy} /> Clear the saved MPD password</label>
    <div class="field-actions"><button type="submit" class="primary-button" disabled={busy}>{busy ? "Saving…" : "Save connection"}</button></div>
    {#if message}<p class="connection-status" class:failed role={failed ? "alert" : "status"}>{message}</p>{/if}
  </form>
{/if}

<style>
  .connection-editor{background:var(--panel-strong);border-radius:8px;display:grid;gap:.75rem;margin:.75rem 0 0;padding:.9rem;flex-basis:100%}.connection-editor-head{align-items:center;display:flex;justify-content:space-between;gap:.7rem}.connection-editor .admin-hint{margin:0}.field-grid{display:flex;flex-wrap:wrap;gap:.7rem}.field-host{flex:1 1 17ch;max-width:22rem}.field-port{flex:0 0 6.5rem;max-width:6.5rem}.field-password{flex:1 1 18ch;max-width:22rem}.clear-password{color:var(--muted);display:flex;font-size:.82rem;gap:.5rem;align-items:center}.connection-status{color:var(--ok);margin:0}.connection-status.failed{color:var(--danger);font-family:var(--font-mono)}
</style>
