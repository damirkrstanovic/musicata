// Zones use the real player/zone API. Snapcast is mocked only at the browser boundary so this
// regression remains runnable on a smoke server without physical speakers.
export async function zonesFlows({api, js, check, waitUntil, tap}) {
  const zoneName = "UI zone created";
  const renamed = "UI zone renamed";
  const mock = [
    {id:"snap-ui-kitchen",name:"Kitchen",hostname:"kitchen-speaker",ip_address:"192.0.2.36",zone_id:null,connected:true,volume_percent:50,muted:false},
    {id:"snap-ui-office",name:"Office",hostname:"office-speaker",ip_address:"192.0.2.48",zone_id:null,connected:false,volume_percent:50,muted:false},
  ];
  await js(`(() => {
    const nativeFetch = window.fetch.bind(window);
    window.__zonesNativeFetch = nativeFetch;
    window.__zonesClients = ${JSON.stringify(mock)};
    window.fetch = async (input, init = {}) => {
      const url = String(input), method = (init.method || "GET").toUpperCase();
      if (url.includes("/api/snapcast/clients/snap-ui-") && method === "PATCH") {
        const id = decodeURIComponent(url.split("/").pop());
        const patch = JSON.parse(init.body || "{}");
        window.__zonesClients = window.__zonesClients.map(client => client.id === id ? {...client, ...patch} : client);
        return new Response("{}", {status: 200, headers: {"content-type":"application/json"}});
      }
      if (url.includes("/api/zones/") && method === "DELETE") {
        const zoneId = decodeURIComponent(url.split("/").pop());
        const response = await nativeFetch(input, init);
        if (response.ok) window.__zonesClients = window.__zonesClients.map(client => client.zone_id === zoneId ? {...client, zone_id:null} : client);
        return response;
      }
      if (url.includes("/api/snapcast/status") && method === "GET") {
        let base = {};
        try { const response = await nativeFetch(input, init); if (response.ok) base = await response.json(); } catch { /* fixture below */ }
        const clients = window.__zonesClients;
        return new Response(JSON.stringify({...base, clients, health: {...base.health, control:"connected", clients}, inputs:base.inputs || [], groups:base.groups || []}), {status: 200, headers: {"content-type":"application/json"}});
      }
      return nativeFetch(input, init);
    };
  })()`);
  try {
  await tap('[aria-label="Settings"]');
  await waitUntil(`document.querySelector('[data-settings-category="outputs"]')`, 3000);
  await js(`document.querySelector('[data-settings-category="outputs"]')?.click()`);
  check("Zones: outputs settings open", await waitUntil(`document.querySelector('[data-settings-panel="outputs"] .zones-panel')`, 3000) < Infinity);
  await waitUntil(`document.querySelector('[aria-label="Zone for Kitchen"]')`, 3000);
  check("Zones: unzoned Snapcast speakers use shared playback", await js(`(() => {
    const shared = [...document.querySelectorAll('.shared-playback')].find(section => section.textContent.includes('Shared playback'));
    const unassigned = document.querySelector('.unassigned-outputs');
    return shared?.textContent.includes('Kitchen')
      && shared?.textContent.includes('Office')
      && shared?.textContent.includes('default queue')
      && !unassigned?.textContent.includes('Kitchen')
      && !unassigned?.textContent.includes('Office');
  })()`));
  check("Zones: Snapcast zone menus name shared playback", await js(`
    [...document.querySelector('[aria-label="Zone for Kitchen"]')?.options || []]
      .some(option => option.value === '' && option.textContent.trim() === 'Shared playback')
  `));
  check("Zones: independent players remain unassigned", await js(`
    [...document.querySelector('[aria-label="Zone for Remote audio"]')?.options || []]
      .some(option => option.value === '' && option.textContent.trim() === 'No zone')
  `));

  await js(`(() => { const form = document.querySelector('.zone-create'); form.querySelector('input').value = ${JSON.stringify(zoneName)}; form.requestSubmit(); })()`);
  check("Zones: create uses the admin form", await waitUntil(`[...document.querySelectorAll('.zone-card h3')].some(node => node.textContent === ${JSON.stringify(zoneName)})`, 3000) < Infinity);
  const created = (await api("/api/zones")).find(zone => zone.name === zoneName);

  if (!created) {
    check("Zones: created zone is available through the API", false);
  } else {
    // MPD refresh can finish after the new card is visible. Wait for the save to release
    // the form before clicking Manage, including the offline-player timeout path.
    await waitUntil(`document.querySelector('[aria-label="Zone for Office"]:not(:disabled)')`, 15000);
    await js(`(() => { const card = [...document.querySelectorAll('.zone-card')].find(node => node.querySelector('h3')?.textContent === ${JSON.stringify(zoneName)}); card.querySelector('button')?.click(); })()`);
    check("Zones: manage opens inline membership editor", await waitUntil(`document.querySelector('.zone-editor input')`, 2000) < Infinity);
    await js(`(() => { const input=document.querySelector('.zone-editor input'); input.value=${JSON.stringify(renamed)}; input.dispatchEvent(new Event('input',{bubbles:true})); [...document.querySelectorAll('.zone-editor .membership-list label')].find(label => label.textContent.includes('Kitchen'))?.querySelector('input')?.click(); document.querySelector('.zone-editor')?.requestSubmit(); })()`);
    check("Zones: rename and bulk speaker membership save", await waitUntil(`(() => { const card=[...document.querySelectorAll('.zone-card')].find(node => node.querySelector('h3')?.textContent === ${JSON.stringify(renamed)}); return card?.textContent.includes('Kitchen') && card?.textContent.includes('192.0.2.36'); })()`, 3000) < Infinity);

    // Saving refreshes MPD state/processor bindings after the rows appear. Wait until the
    // form is interactive before dispatching the next change, just as a user must.
    await waitUntil(`document.querySelector('[aria-label="Zone for Office"]:not(:disabled)')`, 15000);
    await js(`(() => { const select=document.querySelector('[aria-label="Zone for Office"]'); select.value=${JSON.stringify(created.id)}; select.dispatchEvent(new Event('change',{bubbles:true})); })()`);
    check("Zones: physical speaker assignment moves the visible row", await waitUntil(`(() => { const card=[...document.querySelectorAll('.zone-card')].find(node => node.querySelector('h3')?.textContent === ${JSON.stringify(renamed)}); return card?.textContent.includes('Office') && card?.textContent.includes('192.0.2.48'); })()`, 3000) < Infinity);

    await waitUntil(`document.querySelector('[aria-label="Zone for Office"]:not(:disabled)')`, 15000);
    await js(`(() => { const card = [...document.querySelectorAll('.zone-card')].find(node => node.querySelector('h3')?.textContent === ${JSON.stringify(renamed)}); [...card.querySelectorAll('button')].find(button => button.textContent === 'Delete')?.click(); })()`);
    await waitUntil(`document.querySelector('.modal[role="dialog"]')`, 2000);
    await js(`document.querySelector('.modal .primary-button')?.click()`);
    check("Zones: delete returns speakers to shared playback", await waitUntil(`(() => {
      const shared = document.querySelector('.shared-playback');
      return ![...document.querySelectorAll('.zone-card h3')].some(node => node.textContent === ${JSON.stringify(renamed)})
        && shared?.textContent.includes('Kitchen')
        && shared?.textContent.includes('Office')
        && [...document.querySelector('[aria-label="Zone for Kitchen"]')?.options || []].some(option => option.value === '' && option.textContent.trim() === 'Shared playback');
    })()`, 3000) < Infinity);
  }
  } finally {
    await js(`window.fetch = window.__zonesNativeFetch; delete window.__zonesNativeFetch; delete window.__zonesClients`);
    await js(`document.querySelector('[data-activity="browse"]')?.click()`);
    check("Zones: returns to the music library", await waitUntil(`document.querySelector(".album-card") || document.querySelector(".track")`, 3000) < Infinity);
  }
}
