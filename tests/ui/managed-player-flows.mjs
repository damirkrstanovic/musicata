// Managed Snapclient device pairing is a server-mediated flow; this smoke test supplies only
// the Snapcast status observation. Pairing, enrollment, reports and revocation remain real API
// calls so a stale mock cannot mask a protocol regression.
export async function managedPlayerFlows({api, js, check, waitUntil, send, screenshot}) {
  const clientId = 'managed-smoke-client';
  const clientName = 'Managed smoke speaker';
  const row = `([...document.querySelectorAll('.snapcast-client')].find(row => row.textContent.includes(${JSON.stringify(clientName)})))`;
  const report = async (token, body) => api('/api/managed-players/report', {
    method: 'POST', headers: {'content-type': 'application/json', authorization: `Bearer ${token}`}, body: JSON.stringify(body),
  });
  const inventory = (overrides = {}) => ({
    inventory_revision: 'inventory-one',
    devices: [
      {id: 'usb-dac', name: 'USB DAC', available: true},
      {id: 'hdmi', name: 'HDMI output', available: true},
    ],
    selected_device_id: 'usb-dac', applied_revision: 0, attempted_revision: 0,
    status: 'idle', error: null, selection_supported: true, ...overrides,
  });
  let token = null;

  try {
    await js(`(() => {
      window.__managedFetch = window.fetch.bind(window);
      window.__managedView = null;
      window.fetch = async (input, init = {}) => {
        const url = input instanceof Request ? input.url : String(input);
        const method = (init.method || 'GET').toUpperCase();
        if (url.includes('/api/snapcast/status') && method === 'GET') {
          const response = await window.__managedFetch(input, init);
          const base = await response.json();
          const client = {id:${JSON.stringify(clientId)},name:${JSON.stringify(clientName)},connected:true,volume_percent:40,muted:false,hostname:'smoke.local',zone_id:null};
          return new Response(JSON.stringify({...base,
            clients:[...(base.clients || []), client],
            health:{...base.health, control:'connected', clients:[...(base.health?.clients || []), client]},
          }), {status:response.status, headers:{'content-type':'application/json'}});
        }
        if (window.__managedView && url.endsWith('/api/managed-players/${clientId}') && method === 'GET') return new Response(JSON.stringify(window.__managedView), {headers:{'content-type':'application/json'}});
        return window.__managedFetch(input, init);
      };
    })()`);
    await js(`document.querySelector('[data-activity="settings"]')?.click()`);
    await waitUntil(`document.querySelector('[data-settings-category="outputs"]')`, 3000);
    await js(`document.querySelector('[data-settings-category="outputs"]')?.click()`);
    check('Managed player: fixture Snapcast client is visible', await waitUntil(row, 4000) < Infinity);

    await js(`${row}?.querySelector('.managed-device summary')?.click()`);
    check('Managed player: pairing code stays hidden until Enable is clicked', await js(`${row}?.querySelector('.pairing-code') === null`));
    check('Managed player: panel refresh completes before pairing is enabled', await waitUntil(`[...${row}?.querySelectorAll('button') || []].some(button => button.textContent.trim() === 'Enable device management' && !button.disabled)`, 3000) < Infinity);
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Enable device management')?.click()`);
    check('Managed player: pairing code is created with expiry', await waitUntil(`${row}?.querySelector('.pairing-code code') && ${row}?.textContent.includes('Expires')`, 4000) < Infinity);
    const pairingToken = await js(`${row}?.querySelector('.pairing-code code')?.textContent.trim()`);
    check('Managed player: pairing token is opaque and one-time sized', typeof pairingToken === 'string' && pairingToken.length >= 32);

    const enrolled = await api('/api/managed-players/enroll', {
      method: 'POST', headers: {'content-type':'application/json'}, body: JSON.stringify({pairing_token: pairingToken, client_id: clientId}),
    });
    token = enrolled?.token;
    check('Managed player: pairing enrolls the fixture host', enrolled?.client_id === clientId && typeof token === 'string');

    await report(token, inventory({selection_supported:false, status:'unsupported', error:'USB selection is unavailable on this host.'}));
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Refresh')?.click()`);
    check('Managed player: unsupported USB reason is shown', await waitUntil(`${row}?.textContent.includes('USB selection is unavailable')`, 3000) < Infinity);

    await report(token, inventory());
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Refresh')?.click()`);
    check('Managed player: fresh available HDMI inventory is selectable', await waitUntil(`${row}?.textContent.includes('HDMI output')`, 3000) < Infinity);
    await js(`[...${row}?.querySelectorAll('.managed-options label')].find(label => label.textContent.includes('HDMI output'))?.querySelector('input')?.click()`);
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Apply audio device')?.click()`);
    check('Managed player: selecting HDMI shows pending state', await waitUntil(`${row}?.textContent.includes('Waiting for the player to apply')`, 3000) < Infinity);

    const desiredOne = await report(token, inventory({attempted_revision:1, applied_revision:1, status:'applied', selected_device_id:'hdmi'}));
    check('Managed player: host receives HDMI desired revision', desiredOne?.revision === 1 && desiredOne?.device_id === 'hdmi');
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Refresh')?.click()`);
    check('Managed player: applied report clears pending state', await waitUntil(`${row}?.textContent.includes('Applied')`, 3000) < Infinity);

    await js(`[...${row}?.querySelectorAll('.managed-options label')].find(label => label.textContent.includes('USB DAC'))?.querySelector('input')?.click()`);
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Apply audio device')?.click()`);
    check('Managed player: second selection reaches pending revision before failure report', await waitUntil(`${row}?.textContent.includes('Waiting for the player to apply')`, 3000) < Infinity);
    await report(token, inventory({attempted_revision:2, applied_revision:1, status:'failed', error:'ALSA rejected the selected device.'}));
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Refresh')?.click()`);
    check('Managed player: failed apply error is surfaced', await waitUntil(`${row}?.textContent.includes('ALSA rejected the selected device')`, 3000) < Infinity);

    const stale = {paired:true,stale:true,last_seen:1,desired:{revision:2,device_id:'usb-dac',inventory_revision:'inventory-one'},report:inventory({attempted_revision:2,applied_revision:1,status:'failed',error:'ALSA rejected the selected device.'})};
    await js(`window.__managedView = ${JSON.stringify(stale)}`);
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Refresh')?.click()`);
    check('Managed player: stale inventory has no enabled selection controls', await waitUntil(`${row}?.textContent.includes('Refresh after the player reports')`, 3000) < Infinity && await js(`!${row}?.querySelector('.managed-device fieldset') || ${row}?.querySelector('.managed-device fieldset')?.disabled === true`));

    for (const [width, height] of [[1440, 900], [390, 844]]) {
      await send('Emulation.setDeviceMetricsOverride', {width, height, deviceScaleFactor:1, mobile:width < 720});
      await js(`${row}?.scrollIntoView({block:'start'})`);
      check(`Managed player: ${width}px has no horizontal overflow`, await js('document.documentElement.scrollWidth <= innerWidth'));
      await screenshot(`managed-player-${width}`);
    }
    await send('Emulation.setDeviceMetricsOverride', {width:1280, height:900, deviceScaleFactor:1, mobile:false});

    await js(`window.__managedView = null`);
    await js(`[...${row}?.querySelectorAll('button') || []].find(button => button.textContent.trim() === 'Disable management')?.click()`);
    check('Managed player: revoke asks for confirmation', await waitUntil(`document.querySelector('.modal[role="dialog"]')?.textContent.includes('Disable device management')`, 2000) < Infinity);
    await js(`[...document.querySelectorAll('.modal[role="dialog"] button')].find(button => button.textContent.trim() === 'Disable')?.click()`);
    check('Managed player: revoke returns to unpaired state', await waitUntil(`${row}?.textContent.includes('Enable device management')`, 4000) < Infinity);
    token = null;
  } finally {
    await js(`if (window.__managedFetch) { window.fetch = window.__managedFetch; delete window.__managedFetch; delete window.__managedView; }`);
    await api(`/api/managed-players/${clientId}`, {method:'DELETE'});
    await send('Emulation.setDeviceMetricsOverride', {width:1280, height:900, deviceScaleFactor:1, mobile:false});
    await js(`document.querySelector('[data-activity="browse"]')?.click()`);
  }
}
